//! Run alongside real playback: cargo run --example scrobble -- COOKIE_FILE UID SONG_ID TOTAL_SECONDS
use netease_cloud_music_api::{
    CookieBuilder, CookieJar, MusicApi, PlaybackEvent, PlaybackEventKind, PlaybackSource,
};
use std::{
    io,
    time::{Duration, Instant, SystemTime},
};

#[async_std::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() == 5,
        "usage: scrobble COOKIE_FILE UID SONG_ID TOTAL_SECONDS"
    );
    let jar = CookieJar::new();
    let uri = "https://music.163.com/".parse()?;
    for part in std::fs::read_to_string(&args[1])?.trim().split(';') {
        if let Some((name, value)) = part.trim().split_once('=') {
            let cookie = CookieBuilder::new(name.trim(), value.trim())
                .domain("music.163.com")
                .path("/")
                .secure(true)
                .build()?;
            jar.set(cookie, &uri)?;
        }
    }
    let mut api = MusicApi::from_cookie_jar(jar, 2);
    if let Ok(proxy) = std::env::var("SCROBBLE_PROXY") {
        api.set_proxy(&proxy)?;
    }
    let context = api.scrobble_context(args[2].parse()?)?;
    println!("Start actual playback in your player, then press Enter. Keep playback continuous for this example.");
    io::stdin().read_line(&mut String::new())?;
    let start = Instant::now();
    let mut event = PlaybackEvent {
        session_id: 1,
        kind: PlaybackEventKind::Start,
        song_id: args[3].parse()?,
        duration: Duration::from_secs(args[4].parse()?),
        played: Duration::ZERO,
        source: PlaybackSource::default(),
        bitrate: 0,
        quality: String::new(),
        timestamp: SystemTime::now(),
    };
    let receipt = api.scrobble(&context, &event).await?;
    println!(
        "Start upload acknowledged: {}. Press Enter when actual playback ends.",
        receipt.file_name
    );
    io::stdin().read_line(&mut String::new())?;
    event.kind = PlaybackEventKind::End;
    event.played = start.elapsed().min(event.duration);
    event.timestamp = SystemTime::now();
    let receipt = api.scrobble(&context, &event).await?;
    println!(
        "End upload acknowledged: {}, measured {} seconds. Check official statistics separately.",
        receipt.file_name,
        event.played.as_secs()
    );
    Ok(())
}
