//! QR codes for the terminal (`cutex management pair`): one character per
//! module column and two module rows (`▀`, foreground = upper module,
//! background = lower), dark on white with a quiet zone.

use qrcode::{Color, EcLevel, QrCode};

const QUIET: usize = 2;
const DARK_FG: &str = "\x1b[30m";
const DARK_BG: &str = "\x1b[40m";
const LIGHT_FG: &str = "\x1b[97m";
const LIGHT_BG: &str = "\x1b[107m";
const RESET: &str = "\x1b[0m";

pub(crate) fn render(payload: &str) -> anyhow::Result<String> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L)
        .map_err(|error| anyhow::anyhow!("QR encoding failed: {error}"))?;
    let width = code.width();
    let size = width + 2 * QUIET;
    let dark = |x: usize, y: usize| -> bool {
        x >= QUIET
            && y >= QUIET
            && x < QUIET + width
            && y < QUIET + width
            && code[(x - QUIET, y - QUIET)] == Color::Dark
    };
    let mut out = String::new();
    let mut y = 0;
    while y < size {
        for x in 0..size {
            let top = dark(x, y);
            let bottom = y + 1 < size && dark(x, y + 1);
            let lower_bg = if y + 1 < size {
                if bottom {
                    DARK_BG
                } else {
                    LIGHT_BG
                }
            } else {
                LIGHT_BG
            };
            out.push_str(if top { DARK_FG } else { LIGHT_FG });
            out.push_str(lower_bg);
            out.push('▀');
        }
        out.push_str(RESET);
        out.push('\n');
        y += 2;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_square_block() {
        let text = super::render("cutex-pair:v1?h=WINBOX&c=ABCD2345").unwrap();
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows.len() > 10);
        let columns = rows[0].matches('▀').count();
        assert!(rows.iter().all(|row| row.matches('▀').count() == columns));
        assert_eq!(rows.len(), columns.div_ceil(2));
    }
}
