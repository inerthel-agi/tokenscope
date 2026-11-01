//! Background loop: tray tooltip, quota notifications and a pressure badge on the icon.
//! Alerts use hysteresis (alert at >=90% used /
//! <=10% left, re-arm below 85% / above 15%) so a window hovering at the line does not spam.
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;
use tauri::image::Image;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Badge {
    None,
    Warn,
    Bad,
}

pub struct Report {
    pub tooltip: String,
    pub badge: Badge,
    pub notify: Vec<(String, String)>,
}

/// Pure step: what to show given the latest utilization and the armed alerts.
pub fn evaluate(u: &Value, alerted: &mut HashSet<String>) -> Report {
    let mut parts = vec!["TokenScope".to_string()];
    let mut worst: f64 = 0.0;
    let mut notify = vec![];
    for l in u["claude"]["limits"].as_array().into_iter().flatten() {
        let Some(p) = l["used_percent"].as_f64() else { continue };
        let lbl = l["label"].as_str().unwrap_or("?");
        if lbl == "5h" {
            parts.push(format!("C 5h {}%", p.round()));
        }
        worst = worst.max(p);
        let key = format!("claude:{lbl}");
        if p >= 90.0 && !alerted.contains(&key) {
            alerted.insert(key);
            notify.push(("TokenScope — Claude".into(), format!("'{lbl}' window at {}% used", p.round())));
        } else if p < 85.0 {
            alerted.remove(&key);
        }
    }
    for (key, lbl) in [("primary", "5h"), ("secondary", "weekly")] {
        let Some(used) = u["codex"][key]["used_percent"].as_f64() else { continue };
        let rem = 100.0 - used;
        if key == "primary" {
            parts.push(format!("X 5h {}%", used.round()));
        }
        worst = worst.max(used);
        let akey = format!("codex:{key}");
        if rem <= 10.0 && !alerted.contains(&akey) {
            alerted.insert(akey);
            notify.push(("TokenScope — Codex".into(), format!("'{lbl}' window: {}% left", rem.round())));
        } else if rem > 15.0 {
            alerted.remove(&akey);
        }
    }
    let mut tooltip = parts.join(" · ");
    tooltip.truncate(127);
    let badge = if worst >= 90.0 { Badge::Bad } else if worst >= 75.0 { Badge::Warn } else { Badge::None };
    Report { tooltip, badge, notify }
}

/// App icon with a colored dot top-right (256-px base geometry).
pub fn badge_icon(base: &Image<'_>, badge: Badge) -> Image<'static> {
    let (w, h) = (base.width(), base.height());
    let mut px = base.rgba().to_vec();
    let color = match badge {
        Badge::None => return Image::new_owned(px, w, h),
        Badge::Warn => [255, 204, 0],
        Badge::Bad => [255, 59, 48],
    };
    let s = w as f64 / 256.0;
    let (cx, cy, r, ring) = (202.0 * s, 54.0 * s, 32.0 * s, 8.0 * s);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
            if d <= r {
                let i = ((y * w + x) * 4) as usize;
                let c = if d > r - ring { [0, 0, 0] } else { color };
                px[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
    }
    Image::new_owned(px, w, h)
}

pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        let mut alerted = HashSet::new();
        let mut last = None;
        let base = app.default_window_icon().cloned();
        loop {
            let rep = evaluate(&crate::util::read_local_utilization(), &mut alerted);
            if let Some(tray) = app.tray_by_id("main") {
                let _ = tray.set_tooltip(Some(&rep.tooltip));
                if last != Some(rep.badge) {
                    if let Some(b) = &base {
                        let _ = tray.set_icon(Some(badge_icon(b, rep.badge)));
                    }
                    last = Some(rep.badge);
                }
            }
            for (title, body) in rep.notify {
                let _ = app.notification().builder().title(title).body(body).show();
            }
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hysteresis_and_badge() {
        let mut armed = HashSet::new();
        let u = |c: f64, x: f64| json!({"claude":{"limits":[{"label":"5h","used_percent":c}]},
                                         "codex":{"primary":{"used_percent":x}}});
        let r = evaluate(&u(91.0, 10.0), &mut armed);
        assert_eq!(r.notify.len(), 1);
        assert_eq!(r.badge, Badge::Bad);
        assert_eq!(r.tooltip, "TokenScope · C 5h 91% · X 5h 10%");
        assert!(evaluate(&u(88.0, 10.0), &mut armed).notify.is_empty()); // still armed above 85
        assert!(evaluate(&u(84.0, 10.0), &mut armed).notify.is_empty()); // re-armed
        assert_eq!(evaluate(&u(92.0, 95.0), &mut armed).notify.len(), 2);
        assert_eq!(evaluate(&u(80.0, 0.0), &mut armed).badge, Badge::Warn);
        assert_eq!(evaluate(&u(10.0, 0.0), &mut armed).badge, Badge::None);
    }

    #[test]
    fn badge_draws_dot() {
        let base = Image::new_owned(vec![0; 256 * 256 * 4], 256, 256);
        let img = badge_icon(&base, Badge::Bad);
        let i = (54 * 256 + 202) * 4;
        assert_eq!(&img.rgba()[i..i + 4], &[255, 59, 48, 255]);
    }
}
