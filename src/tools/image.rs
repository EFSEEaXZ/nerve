//! `image`: show images to the model within the client's size budget, with crop/tile/zoom.
//! `encode_fit` / `view_into` are reused by the desktop (screenshot) module.

use std::io::Cursor;

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::mcp::{OrErr, ToolCtx, ToolDef, ToolOutput, blocking, tool};
use crate::util::{self, fmt_bytes};

#[derive(Deserialize, JsonSchema)]
pub struct ImageArgs {
    /// view (default) | info
    action: Option<String>,
    /// Image file (png, jpg, webp, gif, bmp, tiff, ico) or http(s) URL.
    #[serde(alias = "file", alias = "url")]
    path: Option<String>,
    /// Several images; they share the byte budget.
    paths: Option<Vec<String>>,
    /// Crop [x, y, w, h] in pixels, or as fractions 0..1 (e.g. [0.5, 0, 0.5, 0.5] = top-right quarter).
    region: Option<Vec<f64>>,
    /// Grid tile "CxR:K" (K = 1..C*R, row-major), or "CxR" for all tiles.
    tile: Option<String>,
    /// Extra zoom after cropping (e.g. 2 to enlarge small text; max 4).
    scale: Option<f64>,
    /// Byte budget for the returned image(s) (default from config).
    max_bytes: Option<usize>,
    /// Max width/height of the returned image (default from config).
    max_dim: Option<u32>,
    /// auto (PNG when it fits, else JPEG) | png | jpeg
    format: Option<String>,
    /// Lowest JPEG quality before shrinking instead (default from config).
    min_quality: Option<u8>,
}

const DESC: &str = "View images (files or URLs) inside the client's size budget. \
Picks PNG when it fits, else the best JPEG quality, and only then shrinks. \
For full detail of large images zoom in: region [x,y,w,h] (pixels or 0..1 fractions) or tile \"2x2:1\"; scale enlarges small text. \
The text part reports original size and the coordinate mapping. action info = dimensions/format only.";

pub fn def() -> ToolDef {
    tool::<ImageArgs, _, _>("image", "View image", DESC, json!({"readOnlyHint": true, "openWorldHint": false}), run)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Fmt {
    Auto,
    Png,
    Jpeg,
}

pub struct Encoded {
    pub data: Vec<u8>,
    pub mime: &'static str,
    pub w: u32,
    pub h: u32,
    pub label: String,
}

fn enc_png(img: &DynamicImage) -> Result<Vec<u8>, String> {
    use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
    let mut v = Vec::new();
    let img = match img {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) | DynamicImage::ImageLuma8(_) | DynamicImage::ImageLumaA8(_) => {
            std::borrow::Cow::Borrowed(img)
        }
        other => std::borrow::Cow::Owned(DynamicImage::ImageRgba8(other.to_rgba8())),
    };
    img.write_with_encoder(PngEncoder::new_with_quality(&mut v, CompressionType::Best, PngFilter::Adaptive)).ctx("png encode")?;
    Ok(v)
}

fn enc_jpeg(rgb: &image::RgbImage, q: u8) -> Result<Vec<u8>, String> {
    let mut v = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut v, q);
    rgb.write_with_encoder(enc).ctx("jpeg encode")?;
    Ok(v)
}

/// Drop alpha by compositing onto white (JPEG has no transparency).
fn flatten(img: &DynamicImage) -> image::RgbImage {
    if !img.color().has_alpha() {
        return img.to_rgb8();
    }
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = image::RgbImage::new(w, h);
    for (o, p) in out.pixels_mut().zip(rgba.pixels()) {
        let a = p[3] as u32;
        for i in 0..3 {
            o[i] = ((p[i] as u32 * a + 255 * (255 - a)) / 255) as u8;
        }
    }
    out
}

fn resize(img: &DynamicImage, w: u32, h: u32) -> DynamicImage {
    let (ow, _) = img.dimensions();
    let filter = if w > ow {
        FilterType::CatmullRom
    } else if (w as f64) < ow as f64 * 0.3 {
        FilterType::Triangle
    } else {
        FilterType::Lanczos3
    };
    img.resize_exact(w.max(1), h.max(1), filter)
}

fn scaled(img: &DynamicImage, f: f64) -> DynamicImage {
    let (w, h) = img.dimensions();
    resize(img, ((w as f64 * f).round() as u32).max(1), ((h as f64 * f).round() as u32).max(1))
}

/// Encode `src` so that it fits in `budget` bytes: PNG if it fits (sharp text), otherwise the highest
/// JPEG quality that fits (never below `min_q`), and only then reduce dimensions.
pub fn encode_fit(src: &DynamicImage, budget: usize, max_dim: u32, min_q: u8, fmt: Fmt) -> Result<Encoded, String> {
    let budget = budget.max(2048);
    let (w0, h0) = src.dimensions();
    let big = w0.max(h0);
    let mut img = if max_dim > 0 && big > max_dim { scaled(src, max_dim as f64 / big as f64) } else { src.clone() };
    let min_q = min_q.clamp(20, 95);
    let mut ladder: Vec<u8> = [90u8, 82, 75].into_iter().filter(|q| *q >= min_q).collect();
    if ladder.is_empty() {
        ladder.push(min_q);
    }
    for round in 0..12 {
        let (w, h) = img.dimensions();
        let px = w as usize * h as usize;
        let mut last = 0usize;
        if fmt == Fmt::Png || (fmt == Fmt::Auto && px <= budget * 12) {
            let png = enc_png(&img)?;
            if png.len() <= budget {
                return Ok(Encoded { data: png, mime: "image/png", w, h, label: "png".into() });
            }
            last = png.len();
        }
        if fmt != Fmt::Png {
            let rgb = flatten(&img);
            let qs: Vec<u8> = if round == 0 { ladder.clone() } else { ladder[ladder.len().saturating_sub(2)..].to_vec() };
            for &q in &qs {
                let j = enc_jpeg(&rgb, q)?;
                if j.len() <= budget {
                    return Ok(Encoded { data: j, mime: "image/jpeg", w, h, label: format!("jpeg q{q}") });
                }
                last = j.len();
            }
            if w.max(h) <= 96 {
                let j = enc_jpeg(&rgb, min_q)?;
                return Ok(Encoded { data: j, mime: "image/jpeg", w, h, label: format!("jpeg q{min_q}") });
            }
        }
        let f = ((budget as f64 / last.max(1) as f64).sqrt() * 0.93).clamp(0.2, 0.92);
        img = scaled(&img, f);
    }
    Err("could not fit the image into the byte budget; raise max_bytes".into())
}

#[derive(Clone)]
pub struct ViewOpts {
    pub region: Option<Vec<f64>>,
    pub tile: Option<String>,
    pub scale: Option<f64>,
    pub budget: usize,
    pub max_dim: u32,
    pub fmt: Fmt,
    pub min_q: u8,
    /// Screen position of the image's top-left pixel (screenshots); (0,0) for plain images.
    pub origin: (i32, i32),
    /// Label for the mapped coordinates: "original" (files) or "screen" (screenshots).
    pub coord: &'static str,
}

/// (x, y, w, h) in source pixels
fn region_px(r: &[f64], w: u32, h: u32) -> Result<(u32, u32, u32, u32), String> {
    if r.len() != 4 {
        return Err("region must be [x, y, w, h]".into());
    }
    let frac = r.iter().all(|v| (0.0..=1.0).contains(v)) && r[2] > 0.0 && r[3] > 0.0;
    let (fw, fh) = (w as f64, h as f64);
    let (x, y, rw, rh) = if frac { (r[0] * fw, r[1] * fh, r[2] * fw, r[3] * fh) } else { (r[0], r[1], r[2], r[3]) };
    let x = (x.max(0.0) as u32).min(w.saturating_sub(1));
    let y = (y.max(0.0) as u32).min(h.saturating_sub(1));
    let rw = (rw.max(1.0) as u32).min(w - x);
    let rh = (rh.max(1.0) as u32).min(h - y);
    Ok((x, y, rw, rh))
}

/// Parse "CxR:K" / "CxR" into a list of (x, y, w, h) tiles.
fn tiles(spec: &str, w: u32, h: u32) -> Result<Vec<(u32, u32, u32, u32, String)>, String> {
    let (grid, k) = match spec.split_once(':') {
        Some((g, k)) => (g, Some(k.trim().parse::<u32>().ctx("tile index")?)),
        None => (spec, None),
    };
    let (c, r) = grid.trim().to_ascii_lowercase().split_once('x').map(|(a, b)| (a.trim().parse::<u32>(), b.trim().parse::<u32>())).ok_or("tile must look like \"2x2:1\"")?;
    let (c, r) = (c.ctx("tile columns")?.clamp(1, 8), r.ctx("tile rows")?.clamp(1, 8));
    let all: Vec<u32> = match k {
        Some(k) if k >= 1 && k <= c * r => vec![k],
        Some(k) => return Err(format!("tile index {k} out of range 1..{}", c * r)),
        None if c * r <= 16 => (1..=c * r).collect(),
        None => return Err("at most 16 tiles at once; pick one with \"CxR:K\"".into()),
    };
    Ok(all
        .into_iter()
        .map(|k| {
            let (i, j) = ((k - 1) % c, (k - 1) / c);
            let (x0, x1) = (w * i / c, w * (i + 1) / c);
            let (y0, y1) = (h * j / r, h * (j + 1) / r);
            (x0, y0, x1 - x0, y1 - y0, format!("tile {c}x{r}:{k}"))
        })
        .collect())
}

/// Render one source image (optionally cropped/tiled) into `out` as text metadata + image block(s).
pub fn view_into(img: &DynamicImage, name: &str, o: &ViewOpts, out: &mut ToolOutput) -> Result<(), String> {
    let (w, h) = img.dimensions();
    let mut parts: Vec<(u32, u32, u32, u32, String)> = vec![];
    if let Some(r) = &o.region {
        let (x, y, rw, rh) = region_px(r, w, h)?;
        parts.push((x, y, rw, rh, format!("region {x},{y} {rw}x{rh}")));
    }
    if let Some(t) = &o.tile {
        let base = parts.pop().unwrap_or((0, 0, w, h, String::new()));
        for (x, y, tw, th, label) in tiles(t, base.2, base.3)? {
            parts.push((base.0 + x, base.1 + y, tw, th, format!("{label} = {},{} {tw}x{th}", base.0 + x, base.1 + y)));
        }
    }
    if parts.is_empty() {
        parts.push((0, 0, w, h, String::new()));
    }
    let per = (o.budget / parts.len()).max(4096);
    let zoom = o.scale.unwrap_or(1.0).clamp(0.05, 4.0);
    for (x, y, pw, ph, label) in parts {
        let crop = if (x, y, pw, ph) == (0, 0, w, h) { img.clone() } else { img.crop_imm(x, y, pw, ph) };
        let crop = if (zoom - 1.0).abs() > 0.01 { scaled(&crop, zoom) } else { crop };
        let e = encode_fit(&crop, per, o.max_dim, o.min_q, o.fmt)?;
        let k = pw as f64 / e.w as f64; // source px per sent px
        let mut s = format!("{name} · {w}x{h}");
        if !label.is_empty() {
            s.push_str(&format!(" · {label}"));
        }
        s.push_str(&format!(" · sent {}x{} {} {}", e.w, e.h, e.label, fmt_bytes(e.data.len() as u64)));
        let (ax, ay) = (o.origin.0 + x as i32, o.origin.1 + y as i32);
        if (k - 1.0).abs() > 0.01 || ax != 0 || ay != 0 {
            s.push_str(&format!(" · {} = ({ax} + px*{k:.3}, {ay} + py*{k:.3})", o.coord));
        } else if o.coord == "screen" {
            s.push_str(" · 1:1 screen px");
        }
        if k > 1.6 && o.tile.is_none() {
            s.push_str(&format!(" · downscaled {k:.1}x: zoom with region or tile \"2x2:1\" for detail"));
        }
        out.push_text(s);
        out.push_image(util::b64(&e.data), e.mime);
    }
    Ok(())
}

pub fn decode_bytes(bytes: &[u8]) -> Result<DynamicImage, String> {
    image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ctx("read image")?.decode().ctx("decode image")
}

async fn load(ctx: &ToolCtx, src: &str) -> Result<(Vec<u8>, String), String> {
    if src.starts_with("http://") || src.starts_with("https://") {
        let r = ctx.app.http.get(src).send().await.ctx("download")?;
        if !r.status().is_success() {
            return Err(format!("download failed: HTTP {}", r.status()));
        }
        let b = r.bytes().await.ctx("download")?;
        return Ok((b.to_vec(), src.to_string()));
    }
    let p = util::resolve_path(src, Some(&ctx.app.default_dir.to_string_lossy()));
    let b = tokio::fs::read(&p).await.map_err(|e| format!("{}: {e}", p.display()))?;
    Ok((b, p.display().to_string()))
}

async fn run(a: ImageArgs, ctx: ToolCtx) -> Result<ToolOutput, String> {
    let mut srcs: Vec<String> = a.paths.clone().unwrap_or_default();
    if let Some(p) = &a.path {
        srcs.insert(0, p.clone());
    }
    if srcs.is_empty() {
        return Err("missing required parameter `path` (or `paths`)".into());
    }
    let cfg = &ctx.app.cfg.image;
    let action = a.action.as_deref().unwrap_or("view");
    let fmt = match a.format.as_deref().unwrap_or("auto") {
        "auto" | "" => Fmt::Auto,
        "png" => Fmt::Png,
        "jpeg" | "jpg" => Fmt::Jpeg,
        o => return Err(format!("unknown format `{o}` (auto|png|jpeg)")),
    };
    let opts = ViewOpts {
        region: a.region.clone(),
        tile: a.tile.clone(),
        scale: a.scale,
        budget: a.max_bytes.unwrap_or(cfg.max_bytes).max(4096) / srcs.len(),
        max_dim: a.max_dim.unwrap_or(cfg.max_dim),
        fmt,
        min_q: a.min_quality.unwrap_or(cfg.min_quality),
        origin: (0, 0),
        coord: "original",
    };
    let mut out = ToolOutput::empty();
    let mut failed = 0;
    for src in &srcs {
        let r = async {
            let (bytes, name) = load(&ctx, src).await?;
            match action {
                "info" => {
                    let fmt = image::guess_format(&bytes).map(|f| format!("{f:?}")).unwrap_or_else(|_| "unknown".into());
                    let img = decode_bytes(&bytes)?;
                    let (w, h) = img.dimensions();
                    Ok::<_, String>(vec![format!("{name}: {w}x{h} {fmt} {:?}, {}", img.color(), fmt_bytes(bytes.len() as u64))])
                }
                "view" | "show" => {
                    let o = opts.clone();
                    let part = blocking(move || {
                        let img = decode_bytes(&bytes)?;
                        let mut t = ToolOutput::empty();
                        view_into(&img, &name, &o, &mut t)?;
                        Ok(t)
                    })
                    .await?;
                    out.content.extend(part.content);
                    Ok(vec![])
                }
                o => Err(super::unknown_action("image", o, "view, info")),
            }
        }
        .await;
        match r {
            Ok(lines) => {
                for l in lines {
                    out.push_text(l);
                }
            }
            Err(e) => {
                failed += 1;
                out.push_text(format!("{src}: {e}"));
            }
        }
    }
    Ok(out.error_if(failed == srcs.len()))
}