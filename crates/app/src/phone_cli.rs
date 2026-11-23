//! `sich phone pair | status | devices | revoke <id-prefix> | stop`: console front end of the
//! phone gateway control channel.

use std::time::Duration;

use sich_gateway::{ControlClient, Device};

/// Entry point; returns the process exit code.
pub fn run(base: &str, args: &[String]) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = rt.block_on(async {
        match args.as_slice() {
            ["pair"] => pair(base, false).await,
            ["pair", "--light"] => pair(base, true).await,
            ["status"] => status(base).await,
            ["devices"] => devices(base).await,
            ["revoke", prefix] => revoke(base, prefix).await,
            ["stop"] => stop(base).await,
            _ => {
                eprintln!("usage: sich phone pair [--light] | status | devices | revoke <endpoint-id-prefix> | stop");
                Ok(2)
            }
        }
    });
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    }
}

async fn connect(base: &str) -> anyhow::Result<Option<ControlClient>> {
    Ok(ControlClient::connect_as(base, sich_app::build_id()).await.ok())
}

/// First 8 characters of an endpoint id, for humans.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// The single device whose endpoint id starts with `prefix`.
pub fn resolve_prefix<'a>(devices: &'a [Device], prefix: &str) -> Result<&'a Device, String> {
    if prefix.is_empty() {
        return Err("empty id prefix".into());
    }
    let mut hits = devices.iter().filter(|d| d.endpoint_id.starts_with(prefix));
    match (hits.next(), hits.next()) {
        (None, _) => Err(format!("no paired device starts with `{prefix}`")),
        (Some(d), None) => Ok(d),
        (Some(_), Some(_)) => Err(format!("`{prefix}` matches several devices, give more characters")),
    }
}

/// Is the typed answer a "yes"?
pub fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes" | "т" | "так")
}

/// Renders QR modules (`true` = dark) with unicode half blocks, two module rows per text row,
/// surrounded by a `quiet` module wide light border. By default it assumes a dark terminal
/// (light text on a dark background: light modules are the filled blocks); `invert` is for
/// light terminals.
pub fn render_half_blocks(width: usize, dark: &[bool], quiet: usize, invert: bool) -> String {
    let side = width + 2 * quiet;
    let module = |x: usize, y: usize| -> bool {
        if x < quiet || y < quiet || x >= quiet + width || y >= quiet + width {
            false
        } else {
            dark[(y - quiet) * width + (x - quiet)]
        }
    };
    // "ink" = the glyph is drawn in the text color.
    let ink = |x: usize, y: usize| -> bool {
        if y >= side {
            return !invert; // padding row below an odd height: light
        }
        module(x, y) == invert
    };
    let mut out = String::new();
    for y in (0..side).step_by(2) {
        for x in 0..side {
            out.push(match (ink(x, y), ink(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
    }
    out
}

pub fn render_qr(data: &str, invert: bool) -> anyhow::Result<String> {
    let code = qrcode::QrCode::new(data.as_bytes())?;
    let dark: Vec<bool> = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
    Ok(render_half_blocks(code.width(), &dark, 3, invert))
}

fn format_time(unix_ms: u64) -> String {
    i64::try_from(unix_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".into())
}

async fn status(base: &str) -> anyhow::Result<i32> {
    let Some(mut client) = connect(base).await? else {
        println!("Шлюз не запущено (gateway is not running)");
        return Ok(1);
    };
    let s = client.status().await?;
    println!("Шлюз запущено: версія {}, збірка {}", s.gateway_version, s.build_id);
    println!("Комп'ютер: {}", s.host_name);
    println!("Endpoint id: {}", s.endpoint_id);
    println!("Адреси: {}", if s.addrs.is_empty() { "-".to_string() } else { s.addrs.join(", ") });
    println!("UDP-порт: {}", s.port);
    println!("Relay: {}", s.relay.as_deref().unwrap_or("-"));
    println!("Підключено телефонів: {}", s.connected);
    println!("Спарено пристроїв: {}", s.devices.len());
    if let Some(err) = s.device_list_error {
        println!("УВАГА: список пристроїв нечитабельний, усі телефони відхиляються: {err}");
    }
    Ok(0)
}

async fn devices(base: &str) -> anyhow::Result<i32> {
    let Some(mut client) = connect(base).await? else {
        println!("Шлюз не запущено (gateway is not running)");
        return Ok(1);
    };
    let list = client.list_devices().await?;
    if list.is_empty() {
        println!("Немає спарених пристроїв");
    }
    for d in list {
        println!("{}  {}  спарено {}  востаннє {}", short_id(&d.endpoint_id), d.name, format_time(d.paired_unix_ms), format_time(d.last_seen_unix_ms));
    }
    Ok(0)
}

async fn revoke(base: &str, prefix: &str) -> anyhow::Result<i32> {
    let Some(mut client) = connect(base).await? else {
        println!("Шлюз не запущено (gateway is not running)");
        return Ok(1);
    };
    let list = client.list_devices().await?;
    let device = match resolve_prefix(&list, prefix) {
        Ok(d) => d.clone(),
        Err(e) => {
            eprintln!("{e}");
            return Ok(1);
        }
    };
    client.revoke(&device.endpoint_id).await?;
    println!("Відкликано: {} ({})", device.name, short_id(&device.endpoint_id));
    Ok(0)
}

async fn stop(base: &str) -> anyhow::Result<i32> {
    let Some(mut client) = connect(base).await? else {
        println!("Шлюз не запущено (gateway is not running)");
        return Ok(0);
    };
    client.shutdown().await?;
    println!("Шлюз зупинено");
    Ok(0)
}

async fn read_answer() -> String {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line
    })
    .await
    .unwrap_or_default()
}

async fn pair(base: &str, light: bool) -> anyhow::Result<i32> {
    use std::io::Write as _;
    crate::cli::ensure_gateway(base).await?;
    let mut client = ControlClient::connect_as(base, sich_app::build_id()).await?;
    let info = client.start_pairing().await?;
    println!("Відскануйте QR-код у застосунку Sich на телефоні:\n");
    println!("{}", render_qr(&info.url, light)?);
    println!("{}\n", info.url);
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let ttl = Duration::from_millis(info.expires_unix_ms.saturating_sub(now_ms));
    println!("Код діє {} хв. Чекаю на телефон...", ttl.as_secs().div_ceil(60));
    let request = match tokio::time::timeout(ttl, client.next_pair_request()).await {
        Ok(request) => request?,
        Err(_) => {
            println!("Час очікування вичерпано, код більше не дійсний.");
            return Ok(1);
        }
    };
    let Some(request) = request else {
        println!("Шлюз зупинився.");
        return Ok(1);
    };
    print!("Дозволити {} ({})? [y/N] ", request.device_name, short_id(&request.endpoint_id));
    std::io::stdout().flush().ok();
    let allow = is_yes(&read_answer().await);
    client.confirm(request.request_id, allow).await?;
    if allow {
        println!("Телефон {} спарено.", request.device_name);
        Ok(0)
    } else {
        println!("Відхилено.");
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str) -> Device {
        Device { endpoint_id: id.into(), name: format!("n-{id}"), paired_unix_ms: 0, last_seen_unix_ms: 0 }
    }

    #[test]
    fn short_id_is_eight_chars() {
        assert_eq!(short_id("0123456789abcdef"), "01234567");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn prefix_resolution() {
        let list = [dev("abc123"), dev("abd456"), dev("zzz")];
        assert_eq!(resolve_prefix(&list, "abc").unwrap().endpoint_id, "abc123");
        assert_eq!(resolve_prefix(&list, "zz").unwrap().endpoint_id, "zzz");
        assert!(resolve_prefix(&list, "ab").unwrap_err().contains("several"));
        assert!(resolve_prefix(&list, "q").unwrap_err().contains("no paired"));
        assert!(resolve_prefix(&list, "").is_err());
        assert!(resolve_prefix(&[], "a").is_err());
    }

    #[test]
    fn yes_answers() {
        for yes in ["y", "Y", " yes\n", "YES", "так"] {
            assert!(is_yes(yes), "{yes}");
        }
        for no in ["", "n", "\n", "yep", "no", "maybe"] {
            assert!(!is_yes(no), "{no}");
        }
    }

    #[test]
    fn half_blocks_on_a_dark_terminal() {
        // 2x2 modules: dark at (0,0) and (1,1); no quiet zone.
        let dark = [true, false, false, true];
        // Row pair (y0,y1): column 0 = dark over light -> '▄' (top dark = not ink, bottom light = ink)
        // column 1 = light over dark -> '▀'.
        assert_eq!(render_half_blocks(2, &dark, 0, false), "▄▀\n");
    }

    #[test]
    fn half_blocks_invert_for_light_terminals_and_quiet_zone() {
        let dark = [true];
        // Dark terminal: 1 module + quiet 1 -> 3x3. Light border is ink, the dark module is not.
        assert_eq!(render_half_blocks(1, &dark, 1, false), "█▀█\n███\n");
        // Light terminal: the dark module is ink, the border is not.
        assert_eq!(render_half_blocks(1, &dark, 1, true), " ▄ \n   \n");
    }

    #[test]
    fn qr_for_a_pair_url_renders_square_text() {
        let url = "sich://pair?v=1&id=abc&addrs=1.2.3.4%3A5&relay=&secret=xyz&host=PC";
        let text = render_qr(url, false).unwrap();
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows.len() > 10);
        let width = rows[0].chars().count();
        assert!(rows.iter().all(|r| r.chars().count() == width));
        assert!(text.contains('█'));
    }
}
