//! The Wacom driver's settings: its Tip Feel curve and the "Use Windows Ink" checkbox.
//!
//! The Windows driver (6.4.x) keeps its settings in `%APPDATA%\WTablet\Wacom_Tablet.dat`, a plain
//! XML document keyed per application, with per pen end a Tip Feel curve
//! (`PressureCurveControlPoint`, a cubic Bezier as six numbers in the driver's 0..8191 space: the
//! three control points after an implicit `(0, 0)` start). The driver applies that curve only on
//! its Wintab path — its Windows Ink path (`WM_POINTER`, which winit forwards) delivers the pen's
//! raw pressure untouched. [`load`] reads the curve (and the scope's "Use Windows Ink"
//! checkbox, `WinUseInk`) that applies to this executable. The checkbox is what picks the app's
//! pen path: off, the driver serves Wintab ([`crate::wintab`]) and synthesizes the mouse itself.
//!
//! The file also rewrites itself constantly (the driver records usage statistics); callers read
//! it once at start-up, which is enough — the Ink checkbox needs a driver-settings visit anyway.
//! Everything here is plain file and string handling: no `unsafe`, no dependencies.

use std::fs;
use std::path::{Path, PathBuf};

/// The driver's Tip Feel for one end of the pen: a pressure Bezier, its three control points in
/// 0..1 after an implicit `(0, 0)` start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TipFeel {
    pub curve: [(f32, f32); 3],
}

impl Default for TipFeel {
    /// The driver's Tip Feel "middle" (`327 0 4095 4095 8191 8191`): a near-identity curve, also
    /// the fallback for a machine without the driver or an unreadable settings file.
    fn default() -> Self {
        Self { curve: [(327.0 / 8191.0, 0.0), (4095.0 / 8191.0, 4095.0 / 8191.0), (1.0, 1.0)] }
    }
}

impl TipFeel {
    /// Map a raw pressure 0..1 through the curve: solve it for the parameter whose x is `p` and
    /// report its y. Inputs past the curve's end (a "soft" curve reaches full output before full
    /// input) clamp to the end's height.
    pub fn apply(&self, pressure: f32) -> f32 {
        let [(x1, y1), (x2, y2), (x3, y3)] = self.curve;
        if pressure <= 0.0 || pressure.is_nan() {
            return 0.0;
        }
        if pressure >= x3 {
            return y3;
        }
        // The cubic Bezier from (0, 0) with the three stored control points; x(t) is monotone
        // (the parser rejects anything else), so a bisection from a first guess of `p` finds t.
        let at = |a: f32, b: f32, c: f32, t: f32| 3.0 * t * (1.0 - t) * (1.0 - t) * a + 3.0 * t * t * (1.0 - t) * b + t * t * t * c;
        let (mut lo, mut hi, mut t) = (0.0f32, 1.0f32, pressure);
        for _ in 0..24 {
            let x = at(x1, x2, x3, t);
            if (x - pressure).abs() < 1e-4 {
                break;
            }
            if x < pressure {
                lo = t;
            } else {
                hi = t;
            }
            t = 0.5 * (lo + hi);
        }
        at(y1, y2, y3, t)
    }
}

/// The Tip Feel of a pen: its tip and its eraser end, each with its own curve, and the scope's
/// "Use Windows Ink" checkbox (off: the driver serves Wintab and synthesizes the mouse itself).
#[derive(Clone, Copy, Debug)]
pub struct PenFeel {
    pub tip: TipFeel,
    pub eraser: TipFeel,
    /// `WinUseInk` of the matched scope; the Windows Ink default when the file omits it.
    pub use_ink: bool,
}

impl Default for PenFeel {
    fn default() -> Self {
        Self { tip: TipFeel::default(), eraser: TipFeel::default(), use_ink: true }
    }
}

/// The driver's settings file, `%APPDATA%\WTablet\Wacom_Tablet.dat`, if it exists.
pub fn settings_path() -> Option<PathBuf> {
    let mut path = PathBuf::from(std::env::var_os("APPDATA")?);
    path.push("WTablet");
    path.push("Wacom_Tablet.dat");
    path.is_file().then_some(path)
}

/// Read the Tip Feel that the driver applies (on its Wintab path) to `exe` - the running
/// executable when `None`: the application scope matching its full path, else one matching its
/// file name alone, else the "All" scope. `None` when there is no readable settings file.
pub fn load(exe: Option<&str>) -> Option<PenFeel> {
    let text = fs::read_to_string(settings_path()?).ok()?;
    let exe = exe.map(str::to_owned).or_else(|| std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned()))?;
    let scope = application_scope(&text, &exe)?;
    pen_feel(&text, scope)
}

/// The application scope id the driver's settings pick for `exe` (lowercased on entry).
fn application_scope(text: &str, exe: &str) -> Option<u32> {
    let name = file_name(exe);
    let (mut by_name, mut all) = (None, None);
    let mut rest = text;
    while let Some(i) = rest.find("<AppID") {
        rest = &rest[i..];
        let Some(end) = rest.find("</AppID") else { break };
        let block = &rest[..end];
        rest = &rest[end..];
        let digits: String = block["<AppID".len()..].chars().take_while(char::is_ascii_digit).collect();
        let Ok(id) = digits.parse::<u32>() else { continue };
        let Some(path) = tagged(block, "ApplicationLongName") else { continue };
        let path = path.to_lowercase();
        if path == exe {
            return Some(id);
        }
        if by_name.is_none() && name.is_some() && name == file_name(&path) {
            by_name = Some(id);
        }
        if id == 0 {
            all = Some(id);
        }
    }
    by_name.or(all)
}

/// The Tip Feel of the tablet's pen in the application scope `id` (the first tablet's, on a
/// multi-tablet desk). `None` when the file has no such scope.
fn pen_feel(text: &str, id: u32) -> Option<PenFeel> {
    let start = text.find("<TabletTransducerArray")?;
    let end = text[start..].find("</TabletTransducerArray>")?;
    let mut rest = &text[start..start + end];
    while let Some(i) = rest.find("<ArrayElement") {
        rest = &rest[i..];
        let Some(end) = rest.find("</ArrayElement>") else { break };
        let block = &rest[..end];
        rest = &rest[end..];
        let Some(associated) = tagged(block, "ApplicationAssociated").and_then(|v| v.parse::<u32>().ok()) else { continue };
        if associated != id {
            continue;
        }
        let tip = section(block, "TransducerTipButtonSettings").and_then(tip_feel);
        let eraser = section(block, "TransducerEraserSettings").and_then(tip_feel);
        let use_ink = tagged(block, "WinUseInk").map(|v| v == "true").unwrap_or(true);
        return Some(PenFeel { tip: tip.unwrap_or_default(), eraser: eraser.unwrap_or_default(), use_ink });
    }
    None
}

/// The Tip Feel of one pen end: its `PressureCurveControlPoint` Bezier, normalized from the
/// driver's 0..8191 space. `None` on anything but six numbers forming a monotone curve.
fn tip_feel(text: &str) -> Option<TipFeel> {
    let numbers = tagged(text, "PressureCurveControlPoint")?;
    let mut numbers = numbers.split_whitespace().filter_map(|n| n.parse::<f32>().ok());
    let mut points = [(0.0f32, 0.0f32); 3];
    for point in &mut points {
        *point = (numbers.next()?, numbers.next()?);
    }
    const MAX: f32 = 8191.0;
    let curve = points.map(|(x, y)| ((x / MAX).clamp(0.0, 1.0), (y / MAX).clamp(0.0, 1.0)));
    let [(x1, _), (x2, _), (x3, _)] = curve;
    if x1 < 0.0 || x2 < x1 || x3 < x2 {
        return None;
    }
    Some(TipFeel { curve })
}

/// The text of the first `<Tag ...>value</Tag>` in `text`.
fn tagged<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag} ");
    let start = text.find(&open)? + open.len();
    let start = text[start..].find('>')? + start + 1;
    let end = text[start..].find('<')? + start;
    Some(text[start..end].trim())
}

/// The text between the first `<Tag ...>` and its `</Tag>` in `text`.
fn section<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let start = text.find(&open)? + open.len();
    let close = format!("</{tag}>");
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

/// The lowercased file name of a path (for matching an application scope by name alone).
fn file_name(path: &str) -> Option<String> {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of a real `Wacom_Tablet.dat`, trimmed to what the reader looks at: two scopes
    /// ("All" and one per application) and one tablet's pens, each element holding its own
    /// settings. Tags carry attributes, so `tagged`'s `<Tag ` form matches.
    const DAT: &str = r#"<WacomTablet version="...">
  <AppID0>
    <ApplicationLongName modifier="modified">C:\Apps\Others\SomeOther.exe</ApplicationLongName>
  </AppID0>
  <AppID7>
    <ApplicationLongName modifier="modified">C:\Apps\PhotoCraft\photocraft.exe</ApplicationLongName>
  </AppID7>
  <TabletTransducerArray>
    <ArrayElement>
      <ApplicationAssociated modifier="modified">0</ApplicationAssociated>
      <TransducerTipButtonSettings>
        <PressureCurveControlPoint modifier="modified">819 0 6553 4914 8191 8191</PressureCurveControlPoint>
      </TransducerTipButtonSettings>
    </ArrayElement>
    <ArrayElement>
      <ApplicationAssociated modifier="modified">7</ApplicationAssociated>
      <TransducerTipButtonSettings>
        <PressureCurveControlPoint modifier="modified">327 0 4095 4095 8191 8191</PressureCurveControlPoint>
      </TransducerTipButtonSettings>
      <TransducerEraserSettings>
        <PressureCurveControlPoint modifier="modified">0 0 4095 2047 8191 8191</PressureCurveControlPoint>
      </TransducerEraserSettings>
      <WinUseInk modifier="modified">false</WinUseInk>
    </ArrayElement>
  </TabletTransducerArray>
</WacomTablet>"#;

    #[test]
    fn the_scope_matches_the_full_path_then_the_name_then_all() {
        assert_eq!(application_scope(DAT, r"c:\apps\photocraft\photocraft.exe"), Some(7));
        // A different directory, same file name: the name-only match.
        assert_eq!(application_scope(DAT, r"d:\portable\photocraft.exe"), Some(7));
        // Case-insensitive names: the name-only match again.
        assert_eq!(application_scope(DAT, r"d:\portable\PHOTOCRAFT.EXE"), Some(7));
        // An unknown application: the "All" scope.
        assert_eq!(application_scope(DAT, r"c:\apps\others\reader.exe"), Some(0));
    }

    #[test]
    fn pen_feel_reads_the_curves_and_the_ink_checkbox() {
        let feel = pen_feel(DAT, 7).expect("scope 7 has a pen");
        assert!(!feel.use_ink, "WinUseInk false is the Wintab case");
        assert_eq!(feel.tip.curve, [(327.0 / 8191.0, 0.0), (4095.0 / 8191.0, 4095.0 / 8191.0), (1.0, 1.0)]);
        assert_eq!(feel.eraser.curve, [(0.0, 0.0), (4095.0 / 8191.0, 2047.0 / 8191.0), (1.0, 1.0)]);
        // The "All" scope has no eraser settings (and no WinUseInk line: the driver's default).
        let all = pen_feel(DAT, 0).expect("scope 0 has a pen");
        assert!(all.use_ink);
        assert_eq!(all.tip.curve, [(819.0 / 8191.0, 0.0), (6553.0 / 8191.0, 4914.0 / 8191.0), (1.0, 1.0)]);
        assert_eq!(all.eraser, TipFeel::default());
        assert!(pen_feel(DAT, 9).is_none(), "no pen is associated with an unknown scope");
    }

    #[test]
    fn a_non_monotone_curve_is_rejected() {
        assert!(tip_feel("<PressureCurveControlPoint x=\"1\">8191 4095 4095 4095 8191 8191</PressureCurveControlPoint>").is_none());
        assert!(tip_feel("<PressureCurveControlPoint x=\"1\">1 2 3 4 5</PressureCurveControlPoint>").is_none(), "five numbers");
        assert!(tip_feel("<PressureCurveControlPoint x=\"1\">a b c d e f</PressureCurveControlPoint>").is_none());
    }

    #[test]
    fn tip_feel_maps_pressure_monotonically() {
        // The driver's default ("Tip Feel: middle") is near-identity.
        let f = TipFeel::default();
        assert_eq!(f.apply(0.0), 0.0);
        assert_eq!(f.apply(1.0), 1.0);
        assert!((f.apply(0.5) - 0.5).abs() < 0.01);
        // A "soft" curve reaches full output at half the input; past its end, clamped to its height.
        let soft = TipFeel { curve: [(0.4, 0.0), (0.5, 0.9), (0.6, 1.0)] };
        assert_eq!(soft.apply(0.8), 1.0);
        let mut last = -1.0;
        for step in 1..100 {
            let p = soft.apply(step as f32 / 100.0);
            assert!(p >= last, "monotone at {step}");
            last = p;
        }
    }
}
