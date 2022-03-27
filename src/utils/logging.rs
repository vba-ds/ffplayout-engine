extern crate log;
extern crate simplelog;

use regex::Regex;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use file_rotate::{compression::Compression, suffix::AppendCount, ContentLimit, FileRotate};
use lettre::{transport::smtp::authentication::Credentials, Message, SmtpTransport, Transport};
use log::{Level, LevelFilter, Log, Metadata, Record};
use simplelog::*;
use tokio::runtime::Handle;

use crate::utils::{get_timestamp, GlobalConfig};

pub struct Timer {
    init: Arc<Mutex<bool>>,
    timestamp: Arc<Mutex<i64>>,
    limit: i64,
    messages: Arc<Mutex<Vec<String>>>,
    rt_handle: Handle,
}

impl Timer {
    fn new(rt_handle: Handle) -> Self {
        Self {
            init: Arc::new(Mutex::new(true)),
            timestamp: Arc::new(Mutex::new(get_timestamp())),
            limit: 30 * 1000,
            messages: Arc::new(Mutex::new(vec![])),
            rt_handle,
        }
    }

    fn reset(&self) {
        self.messages.lock().unwrap().clear();
        *self.timestamp.lock().unwrap() = get_timestamp();
    }

    fn queue(&self, msg: String) {
        let now = get_timestamp();
        self.messages.lock().unwrap().push(msg);

        if *self.init.lock().unwrap() {
            self.reset();
            *self.init.lock().unwrap() = false;
        }

        if now >= *self.timestamp.lock().unwrap() + self.limit {
            self.rt_handle.spawn(send_mail(self.messages.lock().unwrap().clone()));

            self.reset();
        }
    }
}

async fn send_mail(messages: Vec<String>) {
    let config = GlobalConfig::global();
    let msg = messages.join("\n");

    let email = Message::builder()
        .from(config.mail.sender_addr.parse().unwrap())
        .to(config.mail.recipient.parse().unwrap())
        .subject(config.mail.subject.clone())
        .body(clean_string(msg.clone()))
        .unwrap();

    let credentials = Credentials::new(
        config.mail.sender_addr.clone(),
        config.mail.sender_pass.clone(),
    );

    let mut transporter = SmtpTransport::relay(config.mail.smtp_server.clone().as_str());

    if config.mail.starttls {
        transporter = SmtpTransport::starttls_relay(config.mail.smtp_server.clone().as_str())
    }

    let mailer = transporter.unwrap().credentials(credentials).build();

    // Send the email
    match mailer.send(&email) {
        Ok(_) => (),
        Err(e) => info!("Could not send email: {:?}", e),
    }
}

pub struct LogMailer {
    level: LevelFilter,
    config: Config,
    timer: Timer,
}

impl LogMailer {
    pub fn new(log_level: LevelFilter, config: Config, timer: Timer) -> Box<LogMailer> {
        Box::new(LogMailer {
            level: log_level,
            config,
            timer,
        })
    }
}

impl Log for LogMailer {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata()) {
            match record.level() {
                Level::Error => {
                    self.timer.queue(record.args().to_string());
                }
                Level::Warn => {
                    self.timer.queue(record.args().to_string());
                }
                _ => (),
            }
        }
    }

    fn flush(&self) {}
}

impl SharedLogger for LogMailer {
    fn level(&self) -> LevelFilter {
        self.level
    }

    fn config(&self) -> Option<&Config> {
        Some(&self.config)
    }

    fn as_log(self: Box<Self>) -> Box<dyn Log> {
        Box::new(*self)
    }
}

fn clean_string(text: String) -> String {
    let regex: Regex = Regex::new(r"\x1b\[[0-9;]*[mGKF]").unwrap();

    regex.replace_all(text.as_str(), "").to_string()
}

pub fn init_logging(rt_handle: Handle) -> Vec<Box<dyn SharedLogger>> {
    let config = GlobalConfig::global();
    let app_config = config.logging.clone();
    let mut time_level = LevelFilter::Off;
    let mut app_logger: Vec<Box<dyn SharedLogger>> = vec![];

    if app_config.timestamp {
        time_level = LevelFilter::Error;
    }

    let log_config = simplelog::ConfigBuilder::new()
        .set_thread_level(LevelFilter::Off)
        .set_target_level(LevelFilter::Off)
        .set_level_padding(LevelPadding::Left)
        .set_time_to_local(app_config.local_time)
        .set_time_level(time_level)
        .clone();

    if app_config.log_to_file {
        let file_config = log_config
            .clone()
            .set_time_format("[%Y-%m-%d %H:%M:%S%.3f]".into())
            .build();
        let mut log_path = "logs/ffplayout.log".to_string();

        if Path::new(&app_config.log_path).is_dir() {
            log_path = Path::new(&app_config.log_path)
                .join("ffplayout.log")
                .display()
                .to_string();
        } else if Path::new(&app_config.log_path).is_file() {
            log_path = app_config.log_path
        } else {
            println!("Logging path not exists!")
        }

        let log = || {
            FileRotate::new(
                log_path,
                AppendCount::new(app_config.backup_count),
                ContentLimit::Lines(1000),
                Compression::None,
            )
        };

        app_logger.push(WriteLogger::new(LevelFilter::Debug, file_config, log()));
    } else {
        let term_config = log_config
            .clone()
            .set_level_color(Level::Debug, Some(Color::Ansi256(12)))
            .set_level_color(Level::Info, Some(Color::Ansi256(10)))
            .set_level_color(Level::Warn, Some(Color::Ansi256(208)))
            .set_level_color(Level::Error, Some(Color::Ansi256(9)))
            .set_time_format_str("\x1b[30;1m[%Y-%m-%d %H:%M:%S%.3f]\x1b[0m")
            .build();

        app_logger.push(TermLogger::new(
            LevelFilter::Debug,
            term_config,
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ));
    }

    if config.mail.recipient.len() > 3 {
        let mut filter = LevelFilter::Error;
        let timer = Timer::new(rt_handle);

        let mail_config = log_config
            .clone()
            .set_time_format_str("[%Y-%m-%d %H:%M:%S%.3f]")
            .build();

        if config.mail.mail_level.to_lowercase() == "warning".to_string() {
            filter = LevelFilter::Warn
        }

        app_logger.push(LogMailer::new(filter, mail_config, timer));
    }

    app_logger
}
