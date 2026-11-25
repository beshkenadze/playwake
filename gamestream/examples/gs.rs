//! Development tool: `gs <state-dir> <address> info | pair <pin> | apps | asset <app-id> <file> | launch <app-id> | cancel |
//! stream <app-id> <seconds> <file> [fps]`. `stream` writes the received video to the
//! file. From second 3 the pad alternates each second between A with the left
//! stick up and the left stick right, and each state is printed when it is sent.
//! The state directory keeps the client identity and the host certificate.

use gamestream::{Host, Identity};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{fs, io::Write, path::Path};

fn main() -> gamestream::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [state, address, command, rest @ ..] = args.as_slice() else {
        return Err("usage: gs <state-dir> <address> info | pair <pin> | apps | asset <app-id> <file> | launch <app-id> | cancel | stream <app-id> <seconds> <file> [fps]".into());
    };
    let state = Path::new(state);
    // The directory holds the client's private key: owner only, from creation.
    fs::DirBuilder::new().recursive(true).mode(0o700).create(state).map_err(|e| e.to_string())?;
    let read = |name: &str| fs::read_to_string(state.join(name)).ok();
    let write = |name: &str, text: &str| {
        fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(state.join(name))
            .and_then(|mut file| file.write_all(text.as_bytes())).map_err(|e| e.to_string())
    };
    let identity = match (read("client.key.pem"), read("client.cert.pem")) {
        (Some(key), Some(certificate)) => Identity::from_pem(&key, &certificate)?,
        _ => {
            let identity = Identity::generate()?;
            write("client.key.pem", &identity.key_pem()?)?;
            write("client.cert.pem", &identity.certificate_pem()?)?;
            identity
        }
    };
    let mut host = Host::new(address);
    host.certificate = read("host.cert.pem");
    let info = host.info(&identity)?;
    match (command.as_str(), rest) {
        ("info", []) => println!("{info:#?}"),
        ("pair", [pin]) => {
            println!("Enter PIN {pin} on the PC…");
            gamestream::pair(&mut host, &identity, pin)?;
            write("host.cert.pem", host.certificate.as_deref().unwrap_or_default())?;
            println!("paired");
        }
        ("apps", []) => for app in host.apps(&identity)? { println!("{}\t{}\thdr={}", app.id, app.title, app.hdr); },
        ("asset", [app, file]) => {
            let image = host.asset(&identity, app.parse().map_err(|_| "app id")?)?;
            fs::write(file, &image).map_err(|e| e.to_string())?;
            println!("{} bytes", image.len());
        }
        ("launch", [app]) => {
            let request = gamestream::Launch { app: app.parse().map_err(|_| "app id")?, width: 1920, height: 1080,
                fps: 60, hdr: false, host_audio: false, end_other: None };
            println!("{}", host.launch(&identity, &info, &request)?.rtsp_url);
        }
        ("cancel", []) => host.cancel(&identity)?,
        ("stream", [app, seconds, file, rest @ ..]) => {
            let fps = rest.first().map_or(Ok(60), |fps| fps.parse()).map_err(|_| "fps")?;
            // `tap=<hex key code>` presses that key at second 2 instead of the pad pattern.
            let tap = rest.get(1).and_then(|tap| u8::from_str_radix(tap.strip_prefix("tap=")?, 16).ok());
            let mut tapped = false;
            let (width, height) = (1920, 1080);
            let request = gamestream::Launch { app: app.parse().map_err(|_| "app id")?, width, height, fps,
                hdr: false, host_audio: false, end_other: None };
            let session = host.launch(&identity, &info, &request)?;
            let config = gamestream::Config { width, height, fps, bitrate_kbps: 20000,
                codec: gamestream::Codec::H264, hdr: false };
            let (stream, events) = gamestream::Stream::start(&host, &session, &config)?;
            let mut out = fs::File::create(file).map_err(|e| e.to_string())?;
            let started = std::time::Instant::now();
            let limit = std::time::Duration::from_secs(seconds.parse().map_err(|_| "seconds")?);
            let (mut frames, mut keys, mut bytes, mut audio, mut first) = (0u32, 0u32, 0usize, 0u32, None);
            let mut sent = gamestream::Gamepad::default();
            while started.elapsed() < limit {
                let second = started.elapsed().as_secs();
                if let Some(key) = tap.filter(|_| second >= 2 && !tapped) {
                    tapped = true;
                    stream.key(key, true, 0);
                    stream.key(key, false, 0);
                }
                let pad = match second {
                    _ if tap.is_some() => gamestream::Gamepad::default(),
                    3.. if second + 2 < limit.as_secs() && second % 2 == 1 =>
                        gamestream::Gamepad { buttons: gamestream::BUTTON_A, left_stick: (0, i16::MAX), ..Default::default() },
                    3.. if second + 2 < limit.as_secs() => gamestream::Gamepad { left_stick: (i16::MAX, 0), ..Default::default() },
                    _ => gamestream::Gamepad::default(),
                };
                if pad != sent {
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
                    println!("{:02}:{:02}.{:03} buttons={:#06x} left={:?}", now.as_secs() / 60 % 60, now.as_secs() % 60,
                        now.subsec_millis(), pad.buttons, pad.left_stick);
                    stream.gamepad(&pad);
                    sent = pad;
                }
                match events.recv_timeout(std::time::Duration::from_millis(200)) {
                    Ok(gamestream::Event::Video(frame)) => {
                        first.get_or_insert_with(|| started.elapsed());
                        frames += 1;
                        keys += u32::from(frame.key);
                        bytes += frame.data.len();
                        out.write_all(&frame.data).map_err(|e| e.to_string())?;
                    }
                    Ok(gamestream::Event::Audio(_)) => audio += 1,
                    Ok(gamestream::Event::Rumble(low, high)) => println!("rumble {low} {high}"),
                    Ok(gamestream::Event::Hdr(display)) => println!("hdr {display:?}"),
                    Ok(gamestream::Event::Ended(result)) => { println!("ended: {result:?}"); break }
                    Err(_) => {}
                }
            }
            stream.stop();
            println!("{frames} frames ({keys} key), {bytes} bytes, {audio} audio packets, first frame after {first:?}");
        }
        _ => return Err("unknown command".into()),
    }
    Ok(())
}
