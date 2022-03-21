use std::{
    io::{prelude::*, BufReader, Error, Read},
    process::{ChildStderr, Command, Stdio},
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread::sleep,
    time::Duration,
};

use process_control::{ChildExt, Terminator};
use tokio::runtime::{Handle, Runtime};

async fn ingest_server(
    dec_setting: Vec<&str>,
    ingest_sender: Sender<[u8; 65424]>,
    proc_terminator: Arc<Mutex<Option<Terminator>>>,
    is_terminated: Arc<Mutex<bool>>,
    rt_handle: Handle,
) -> Result<(), Error> {
    let mut buffer: [u8; 65424] = [0; 65424];
    let filter = "[0:v]fps=25,scale=1024:576,setdar=dar=1.778[vout1]";
    let mut filter_list = vec!["-filter_complex", &filter, "-map", "[vout1]", "-map", "0:a"];
    let mut server_cmd = vec!["-hide_banner", "-nostats", "-v", "level+error"];
    let mut stream_input = vec![
        "-f",
        "live_flv",
        "-listen",
        "1",
        "-i",
        "rtmp://localhost:1936/live/stream",
    ];

    server_cmd.append(&mut stream_input);
    server_cmd.append(&mut filter_list);
    server_cmd.append(&mut dec_setting.clone());

    loop {
        if *is_terminated.lock().unwrap() {
            break;
        }

        let mut server_proc = match Command::new("ffmpeg")
            .args(server_cmd.clone())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Err(e) => {
                panic!("couldn't spawn ingest server: {}", e)
            }
            Ok(proc) => proc,
        };

        let serv_terminator = server_proc.terminator()?;
        *proc_terminator.lock().unwrap() = Some(serv_terminator);

        rt_handle.spawn(stderr_reader(
            server_proc.stderr.take().unwrap(),
            "Server".to_string(),
            proc_terminator.clone(),
            is_terminated.clone(),

        ));

        let ingest_reader = server_proc.stdout.as_mut().unwrap();

        loop {
            match ingest_reader.read_exact(&mut buffer[..]) {
                Ok(length) => length,
                Err(_) => break,
            };

            if let Err(e) = ingest_sender.send(buffer) {
                println!("Ingest server error: {:?}", e);
                break;
            }
        }

        sleep(Duration::from_secs(1));

        if let Err(e) = server_proc.wait() {
            panic!("Server error: {:?}", e)
        };
    }

    Ok(())
}

pub async fn stderr_reader(
    std_errors: ChildStderr,
    suffix: String,
    server_term: Arc<Mutex<Option<Terminator>>>,
    is_terminated: Arc<Mutex<bool>>,
) -> Result<(), Error> {
    // read ffmpeg stderr decoder and encoder instance
    // and log the output

    fn format_line(line: String, level: String) -> String {
        line.replace(&format!("[{}] ", level), "")
    }

    let buffer = BufReader::new(std_errors);

    for line in buffer.lines() {
        let line = line?;

        if line.contains("[info]") {
            println!("[{suffix}] {}", format_line(line, "info".to_string()))
        } else if line.contains("[warning]") {
            println!("[{suffix}] {}", format_line(line, "warning".to_string()))
        } else {
            if suffix != "server" && !line.contains("Input/output error") {
                println!(
                    "[{suffix}] {}",
                    format_line(line.clone(), "level+error".to_string())
                );
            }

            if line.contains("Error closing file pipe:: Broken pipe") {
                *is_terminated.lock().unwrap() = true;

                match &*server_term.lock().unwrap() {
                    Some(serv) => unsafe {
                        if let Ok(_) = serv.terminate() {
                            println!("Terminate server done");
                        }
                    },
                    None => (),
                }
            }
        }
    }

    Ok(())
}

fn main() {
    let decoder_term: Arc<Mutex<Option<Terminator>>> = Arc::new(Mutex::new(None));
    let player_term: Arc<Mutex<Option<Terminator>>> = Arc::new(Mutex::new(None));
    let server_term: Arc<Mutex<Option<Terminator>>> = Arc::new(Mutex::new(None));
    let is_terminated: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

    let runtime = Runtime::new().unwrap();
    let rt_handle = runtime.handle();

    let dec_setting: Vec<&str> = vec![
        "-pix_fmt",
        "yuv420p",
        "-c:v",
        "mpeg2video",
        "-g",
        "1",
        "-b:v",
        "50000k",
        "-minrate",
        "50000k",
        "-maxrate",
        "50000k",
        "-bufsize",
        "25000k",
        "-c:a",
        "s302m",
        "-strict",
        "-2",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-f",
        "mpegts",
        "-",
    ];

    let mut player_proc = match Command::new("ffplay")
        .args([
            "-v",
            "level+error",
            "-hide_banner",
            "-nostats",
            "-i",
            "pipe:0",
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Err(e) => panic!("couldn't spawn ffplay: {}", e),
        Ok(proc) => proc,
    };

    rt_handle.spawn(stderr_reader(
        player_proc.stderr.take().unwrap(),
        "Player".to_string(),
        server_term.clone(),
        is_terminated.clone(),
    ));

    let player_terminator = match player_proc.terminator() {
        Ok(proc) => Some(proc),
        Err(_) => None,
    };
    *player_term.lock().unwrap() = player_terminator;

    let (ingest_sender, ingest_receiver): (Sender<[u8; 65424]>, Receiver<[u8; 65424]>) = channel();

    rt_handle.spawn(ingest_server(
        dec_setting.clone(),
        ingest_sender,
        server_term.clone(),
        is_terminated.clone(),
        rt_handle.clone(),
    ));

    let mut buffer: [u8; 65424] = [0; 65424];
    let mut dec_cmd = vec![
        "-v",
        "level+error",
        "-hide_banner",
        "-nostats",
        "-f",
        "lavfi",
        "-i",
        "testsrc=duration=20:size=1024x576:rate=25",
        "-f",
        "lavfi",
        "-i",
        "anoisesrc=d=20:c=pink:r=48000:a=0.5",
    ];

    dec_cmd.append(&mut dec_setting.clone());

    let mut dec_proc = match Command::new("ffmpeg")
        .args(dec_cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Err(e) => panic!("couldn't spawn ffmpeg: {}", e),
        Ok(proc) => proc,
    };

    rt_handle.spawn(stderr_reader(
        dec_proc.stderr.take().unwrap(),
        "Decoder".to_string(),
        server_term.clone(),
        is_terminated.clone(),
    ));

    let dec_terminator = match dec_proc.terminator() {
        Ok(proc) => Some(proc),
        Err(_) => None,
    };
    *decoder_term.lock().unwrap() = dec_terminator;

    let mut player_writer = player_proc.stdin.as_ref().unwrap();
    let dec_reader = dec_proc.stdout.as_mut().unwrap();

    loop {
        let bytes_len = match dec_reader.read(&mut buffer[..]) {
            Ok(length) => length,
            Err(e) => panic!("Reading error from decoder: {:?}", e),
        };

        if let Ok(receive) = ingest_receiver.try_recv() {
            if let Err(e) = player_writer.write_all(&receive) {
                panic!("Err: {:?}", e)
            };
            continue;
        }

        if let Err(e) = player_writer.write(&buffer[..bytes_len]) {
            panic!("Err: {:?}", e)
        };

        if bytes_len == 0 {
            break;
        }
    }

    *is_terminated.lock().unwrap() = true;

    sleep(Duration::from_secs(1));

    println!("Terminate decoder...");

    match &*decoder_term.lock().unwrap() {
        Some(dec) => unsafe {
            if let Ok(_) = dec.terminate() {
                println!("Terminate decoder done");
            }
        },
        None => (),
    }

    println!("Terminate encoder...");

    match &*player_term.lock().unwrap() {
        Some(enc) => unsafe {
            if let Ok(_) = enc.terminate() {
                println!("Terminate encoder done");
            }
        },
        None => (),
    }

    println!("Terminate server...");

    match &*server_term.lock().unwrap() {
        Some(serv) => unsafe {
            if let Ok(_) = serv.terminate() {
                println!("Terminate server done");
            }
        },
        None => (),
    }

    println!("Terminate done...");
}
