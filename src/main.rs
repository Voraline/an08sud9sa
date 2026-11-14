use std::path::Path;

use voice::{BotOptions, JoinConfig, PlaybackOptions, VoiceClient};

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let mut index = 1usize;
    let mut self_bot = false;
    if arguments.get(index).map(String::as_str) == Some("--selfbot") {
        self_bot = true;
        index += 1;
    }

    if arguments.len() < index + 4 {
        std::process::exit(1);
    }

    let guild_id = arguments[index].clone();
    let channel_id = arguments[index + 1].clone();
    let file = arguments[index + 2].clone();

    let mut bots = Vec::new();
    for (offset, token) in arguments[index + 3..].iter().enumerate() {
        bots.push(BotOptions {
            token: token.clone(),
            name: format!("Bot{}", offset + 1),
            self_bot,
        });
    }

    let config = JoinConfig {
        guild_id,
        channel_id,
        self_deaf: false,
        self_mute: false,
        group: "default".to_string(),
    };

    let mut options = PlaybackOptions::default();
    if let Ok(ffmpeg) = std::env::var("FFMPEG_PATH") {
        options.ffmpeg_path = ffmpeg;
    }
    if let Ok(txtime) = std::env::var("VOICE_TXTIME") {
        options.txtime = txtime == "1";
    }
    if let Ok(debug) = std::env::var("VOICE_DEBUG") {
        options.debug = debug != "0";
    }

    let client = VoiceClient::new(bots, config, options);
    client.set_on_error(|bot, message| {
        eprintln!("[{}] {}", bot, message);
    });
    client.set_on_debug(|message| {
        println!("{}", message);
    });

    if !client.start() {
        eprintln!(
            "Ready {}/{} - {}",
            client.ready_count(),
            client.bot_count(),
            client.last_error()
        );
        if client.ready_count() == 0 {
            std::process::exit(1);
        }
    }

    match client.play_file(Path::new(&file)) {
        Ok(()) => {
            println!("Playing {} on {} bot(s)", file, client.ready_count());
            client.wait_for_finished();
        }
        Err(error) => {
            eprintln!("{}", error);
            std::process::exit(1);
        }
    }
}
