//! OCR via the built-in Windows.Media.Ocr engine (offline, fast, no extra installs).
//! Small text is upscaled before recognition; boxes are mapped back to screen pixels.

use image::DynamicImage;
use image::imageops::FilterType;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::core::HSTRING;

use super::UiArgs;
use super::win::{Rect, capture_target, resolve_target};
use crate::mcp::{OrErr, ToolCtx, ToolOutput};

pub struct Line {
    pub text: String,
    pub rect: Rect,
}

fn engine(lang: Option<&str>) -> Result<OcrEngine, String> {
    if let Some(l) = lang.map(str::trim).filter(|l| !l.is_empty()) {
        let langs = OcrEngine::AvailableRecognizerLanguages().ctx("OCR languages")?;
        let want = l.to_ascii_lowercase();
        for lg in langs {
            let tag = lg.LanguageTag().map(|t| t.to_string()).unwrap_or_default();
            let t = tag.to_ascii_lowercase();
            if t == want || t.starts_with(&format!("{want}-")) || want.starts_with(&t) {
                return OcrEngine::TryCreateFromLanguage(&Language::CreateLanguage(&HSTRING::from(tag.as_str())).ctx("language")?)
                    .ctx("create OCR engine");
            }
        }
        let have: Vec<String> = OcrEngine::AvailableRecognizerLanguages()
            .map(|v| v.into_iter().filter_map(|l| l.LanguageTag().ok()).map(|t| t.to_string()).collect())
            .unwrap_or_default();
        return Err(format!(
            "OCR language `{l}` is not installed (installed: {}). Add it in Settings → Time & language → Language (\"Optical character recognition\")",
            have.join(", ")
        ));
    }
    OcrEngine::TryCreateFromUserProfileLanguages().ctx("create OCR engine (no OCR language installed?)")
}

/// Recognize `img` whose top-left sits at screen `origin`. Returns lines in screen pixels.
pub fn recognize(img: &DynamicImage, origin: (i32, i32), lang: Option<&str>) -> Result<Vec<Line>, String> {
    let eng = engine(lang)?;
    let max = OcrEngine::MaxImageDimension().unwrap_or(10000) as f64;
    let (w, h) = (img.width() as f64, img.height() as f64);
    // Upscale small captures (OCR wants >= ~20px glyphs); cap at the engine maximum.
    let mut f: f64 = if w.max(h) < 1400.0 { 2.0 } else { 1.0 };
    if w.max(h) * f > max {
        f = max / w.max(h);
    }
    let work = if (f - 1.0).abs() > 0.01 {
        img.resize_exact((w * f).round() as u32, (h * f).round() as u32, FilterType::CatmullRom)
    } else {
        img.clone()
    };
    let rgba = work.to_rgba8();
    let (ww, hh) = rgba.dimensions();
    let mut bgra = rgba.into_raw();
    for p in bgra.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    let buf = CryptographicBuffer::CreateFromByteArray(&bgra).ctx("OCR buffer")?;
    let bmp = SoftwareBitmap::CreateCopyFromBuffer(&buf, BitmapPixelFormat::Bgra8, ww as i32, hh as i32).ctx("OCR bitmap")?;
    let bmp = SoftwareBitmap::ConvertWithAlpha(&bmp, BitmapPixelFormat::Bgra8, BitmapAlphaMode::Premultiplied).unwrap_or(bmp);
    let res = eng.RecognizeAsync(&bmp).ctx("OCR")?.get().ctx("OCR")?;
    let mut out = vec![];
    for line in res.Lines().ctx("OCR lines")? {
        let words = line.Words().ctx("OCR words")?;
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for wd in &words {
            if let Ok(r) = wd.BoundingRect() {
                x0 = x0.min(r.X);
                y0 = y0.min(r.Y);
                x1 = x1.max(r.X + r.Width);
                y1 = y1.max(r.Y + r.Height);
            }
        }
        let text = line.Text().map(|t| t.to_string()).unwrap_or_default();
        if text.trim().is_empty() || x0 == f32::MAX {
            continue;
        }
        let k = 1.0 / f as f32;
        out.push(Line {
            text,
            rect: Rect {
                x: origin.0 + (x0 * k).floor() as i32,
                y: origin.1 + (y0 * k).floor() as i32,
                w: ((x1 - x0) * k).ceil() as i32,
                h: ((y1 - y0) * k).ceil() as i32,
            },
        });
    }
    Ok(out)
}

fn scan(a: &UiArgs) -> Result<(Vec<Line>, String, Rect), String> {
    let t = resolve_target(a, false)?;
    let (shot, what) = capture_target(&t)?;
    let lines = recognize(&shot.to_image(), (shot.rect.x, shot.rect.y), a.lang.as_deref())?;
    Ok((lines, what, shot.rect))
}

pub fn run(a: &UiArgs, _ctx: &ToolCtx) -> Result<ToolOutput, String> {
    let t0 = std::time::Instant::now();
    let (lines, what, rect) = scan(a)?;
    let boxes = a.boxes.unwrap_or(true);
    let filt = a.filter.as_deref().map(str::to_lowercase).filter(|f| !f.is_empty());
    let mut s = String::new();
    let mut n = 0;
    for l in &lines {
        if let Some(f) = &filt {
            if !l.text.to_lowercase().contains(f) {
                continue;
            }
        }
        n += 1;
        if boxes {
            let (cx, cy) = l.rect.center();
            s.push_str(&format!("({cx},{cy}) {} {}\n", l.rect.fmt(), l.text));
        } else {
            s.push_str(&l.text);
            s.push('\n');
        }
    }
    let head = format!(
        "OCR {what} {} · {n} lines · {}ms{}\n",
        rect.fmt(),
        t0.elapsed().as_millis(),
        if boxes { " · (center) [x,y wxh] text — click the center" } else { "" }
    );
    Ok(ToolOutput::text(head + &s))
}

/// Best line containing `q` (case-insensitive): exact match first, then the shortest containing line.
pub fn find_text(a: &UiArgs, q: &str) -> Result<Option<(Rect, String)>, String> {
    let (lines, _, _) = scan(a)?;
    let ql = q.to_lowercase();
    let mut best: Option<&Line> = None;
    for l in &lines {
        let t = l.text.to_lowercase();
        if t.trim() == ql {
            return Ok(Some((l.rect, l.text.clone())));
        }
        if t.contains(&ql) && best.is_none_or(|b| b.text.len() > l.text.len()) {
            best = Some(l);
        }
    }
    Ok(best.map(|l| (l.rect, l.text.clone())))
}
