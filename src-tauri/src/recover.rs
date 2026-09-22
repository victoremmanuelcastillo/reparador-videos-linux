//! Reconstruye un MP4 al que le falta el `moov` (descarga cortada, el indice nunca se escribio)
//! leyendo el `mdat` crudo: NAL H.264 con prefijo de longitud + audio AAC intercalado.
//!
//! Alcance: video x264 (H.264 4:2:0, 8 o 10 bit, 1 slice por frame, GOP cerrado) + audio AAC-LC.
//! SPS/PPS se regeneran a partir de las opciones de x264 que el encoder deja en el SEI del primer
//! frame; ancho, alto y bit-depth se buscan decodificando el primer IDR con ffmpeg. El audio no tiene
//! limites de frame sin `moov`, asi que se decodifica todo junto y se recodifica a AAC.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use memmap2::Mmap;

const SC: [u8; 4] = [0, 0, 0, 1];
const MAX_NAL: usize = (1 << 24) - 1;
const MAX_W_MB: u32 = 240;
const TEST_H_MB: u32 = 136;
const STD_FPS: [(u32, u32); 10] = [
    (24000, 1001),
    (24, 1),
    (25, 1),
    (30000, 1001),
    (30, 1),
    (50, 1),
    (60000, 1001),
    (60, 1),
    (15, 1),
    (20, 1),
];

pub type Report<'a> = &'a (dyn Fn(f64, &str) + Sync);

pub struct Recovered {
    pub note: String,
}

// ---------------------------------------------------------------- bits

struct Bits<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Bits { d, p: 0 }
    }
    fn bit(&mut self) -> Option<u32> {
        let b = *self.d.get(self.p >> 3)?;
        let v = (b >> (7 - (self.p & 7))) & 1;
        self.p += 1;
        Some(v as u32)
    }
    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    fn ue(&mut self) -> Option<u32> {
        let mut z = 0;
        while self.bit()? == 0 {
            z += 1;
            if z > 31 {
                return None;
            }
        }
        Some((1u32 << z) - 1 + if z > 0 { self.u(z)? } else { 0 })
    }
}

#[derive(Default)]
struct BitW {
    out: Vec<u8>,
    cur: u8,
    n: u8,
}

impl BitW {
    fn bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | (b as u8 & 1);
        self.n += 1;
        if self.n == 8 {
            self.out.push(self.cur);
            self.cur = 0;
            self.n = 0;
        }
    }
    fn u(&mut self, n: u32, v: u64) {
        for i in (0..n).rev() {
            self.bit(((v >> i) & 1) as u32);
        }
    }
    fn ue(&mut self, v: u32) {
        let v = v as u64 + 1;
        let n = 64 - v.leading_zeros();
        self.u(n - 1, 0);
        self.u(n, v);
    }
    fn se(&mut self, v: i32) {
        self.ue(if v > 0 { (2 * v - 1) as u32 } else { (-2 * v) as u32 });
    }
    fn finish(mut self) -> Vec<u8> {
        self.bit(1);
        while self.n != 0 {
            self.bit(0);
        }
        self.out
    }
}

/// RBSP -> NAL payload (inserta el byte 0x03 de emulation prevention).
fn escape(b: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(b.len() + 4);
    let mut z = 0;
    for &x in b {
        if z >= 2 && x <= 3 {
            o.push(3);
            z = 0;
        }
        o.push(x);
        z = if x == 0 { z + 1 } else { 0 };
    }
    o
}

fn unescape(b: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(b.len());
    let mut z = 0;
    for &x in b {
        if z >= 2 && x == 3 {
            z = 0;
            continue;
        }
        o.push(x);
        z = if x == 0 { z + 1 } else { 0 };
    }
    o
}

// ---------------------------------------------------------------- x264

struct X264 {
    cabac: bool,
    bframes: u32,
    pyramid: u32,
    refs: u32,
    t8x8: bool,
    weightp: u32,
    weightb: bool,
    cqo: i32,
    constrained: bool,
    keyint: u32,
}

fn parse_x264(sei: &[u8]) -> Result<X264, String> {
    let text = String::from_utf8_lossy(sei);
    let idx = text.find("options:").ok_or(
        "Solo se pueden reconstruir videos codificados con x264 (no se encontro su firma en el archivo).",
    )?;
    let s = &text[idx + 8..];
    let s = s.split('\0').next().unwrap_or(s);
    let map: HashMap<&str, &str> = s.split_whitespace().filter_map(|t| t.split_once('=')).collect();
    let num = |k: &str, def: i64| -> i64 {
        map.get(k)
            .and_then(|v| v.split(':').next())
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v as i64)
            .unwrap_or(def)
    };
    if num("interlaced", 0) != 0 || num("cqm", 0) != 0 || num("open_gop", 0) != 0 {
        return Err("Video entrelazado, con matrices CQM u open-GOP: no soportado.".into());
    }
    if num("sliced_threads", 0) != 0 || num("slices", 1) > 1 {
        return Err("Video con varios slices por frame: no soportado.".into());
    }
    Ok(X264 {
        cabac: num("cabac", 1) != 0,
        bframes: num("bframes", 3) as u32,
        pyramid: num("b_pyramid", 2) as u32,
        refs: num("ref", 3) as u32,
        t8x8: num("8x8dct", 1) != 0,
        weightp: num("weightp", 2) as u32,
        weightb: num("weightb", 1) != 0,
        cqo: num("chroma_qp_offset", 0) as i32,
        constrained: num("constrained_intra", 0) != 0,
        keyint: num("keyint", 250) as u32,
    })
}

#[derive(Clone)]
struct Params {
    profile: u8,
    num_ref: u32,
    log2_fn: u32,
    poc_type: u32,
    log2_poc: u32,
    reorder: u32,
}

/// Mismas formulas que `x264_sps_init`.
fn derive(x: &X264, depth: u32) -> Params {
    let pyr = x.pyramid > 0 && x.bframes > 0;
    let reorder = if pyr { 2 } else if x.bframes > 0 { 1 } else { 0 };
    let mut num_ref = x.refs.max(1 + reorder).max(if pyr { 4 } else { 1 });
    if x.pyramid == 1 && x.bframes > 0 {
        num_ref -= 1;
    }
    if x.keyint == 1 {
        num_ref = 0;
    }
    let max_frame_num = num_ref * if pyr { 2 } else { 1 } + 1;
    let mut log2_fn = 4;
    while (1u32 << log2_fn) <= max_frame_num {
        log2_fn += 1;
    }
    let poc_type = if x.bframes > 0 { 0 } else { 2 };
    let mut log2_poc = 4;
    if poc_type == 0 {
        let max_delta = (x.bframes + 2) * if pyr { 2 } else { 1 } * 2;
        while (1u32 << log2_poc) <= max_delta * 2 {
            log2_poc += 1;
        }
    }
    let profile = if depth > 8 {
        110
    } else if x.t8x8 {
        100
    } else if x.cabac || x.bframes > 0 || x.weightp > 0 {
        77
    } else {
        66
    };
    Params { profile, num_ref, log2_fn, poc_type, log2_poc, reorder }
}

fn make_sps(
    p: &Params,
    depth: u32,
    w_mb: u32,
    h_mb: u32,
    level: u8,
    crop: (u32, u32),
    fps: Option<(u32, u32)>,
) -> Vec<u8> {
    let mut w = BitW::default();
    w.u(8, p.profile as u64);
    w.u(8, 0);
    w.u(8, level as u64);
    w.ue(0);
    if matches!(p.profile, 100 | 110 | 122 | 244) {
        w.ue(1);
        w.ue(depth - 8);
        w.ue(depth - 8);
        w.bit(0);
        w.bit(0);
    }
    w.ue(p.log2_fn - 4);
    w.ue(p.poc_type);
    if p.poc_type == 0 {
        w.ue(p.log2_poc - 4);
    }
    w.ue(p.num_ref);
    w.bit(0);
    w.ue(w_mb - 1);
    w.ue(h_mb - 1);
    w.bit(1); // frame_mbs_only
    w.bit(1); // direct_8x8_inference
    if crop != (0, 0) {
        w.bit(1);
        w.ue(0);
        w.ue(crop.0);
        w.ue(0);
        w.ue(crop.1);
    } else {
        w.bit(0);
    }
    match fps {
        Some((num, den)) => {
            w.bit(1); // vui
            for _ in 0..4 {
                w.bit(0); // aspect, overscan, video_signal, chroma_loc
            }
            w.bit(1);
            w.u(32, den as u64);
            w.u(32, 2 * num as u64);
            w.bit(0);
            w.bit(0); // nal_hrd
            w.bit(0); // vcl_hrd
            w.bit(0); // pic_struct
            w.bit(1); // bitstream_restriction
            w.bit(1);
            w.ue(0);
            w.ue(0);
            w.ue(15);
            w.ue(15);
            w.ue(p.reorder);
            w.ue(p.num_ref);
        }
        None => w.bit(0),
    }
    let mut nal = vec![0x67];
    nal.extend(escape(&w.finish()));
    nal
}

fn make_pps(x: &X264, qp: i32) -> Vec<u8> {
    let mut w = BitW::default();
    w.ue(0);
    w.ue(0);
    w.bit(x.cabac as u32);
    w.bit(0);
    w.ue(0);
    w.ue(x.refs.saturating_sub(1)); // num_ref_idx_l0_default_active_minus1 = i_frame_reference - 1
    w.ue(0); // num_ref_idx_l1_default_active_minus1 = 0 (x264 siempre usa l1_default_active = 1)
    w.bit((x.weightp > 0) as u32);
    w.u(2, if x.weightb { 2 } else { 0 });
    w.se(qp - 26);
    w.se(0);
    w.se(x.cqo);
    w.bit(1); // deblocking_filter_control_present
    w.bit(x.constrained as u32);
    w.bit(0);
    if x.t8x8 {
        w.bit(1);
        w.bit(0);
        w.se(x.cqo);
    }
    let mut nal = vec![0x68];
    nal.extend(escape(&w.finish()));
    nal
}

fn pick_level(w: u32, h: u32, num_ref: u32, fps: (u32, u32)) -> u8 {
    // (level_idc, MaxMBPS, MaxFS, MaxDpbMbs)
    const T: [(u8, f64, u32, u32); 16] = [
        (10, 1485.0, 99, 396),
        (11, 3000.0, 396, 900),
        (12, 6000.0, 396, 2376),
        (13, 11880.0, 396, 2376),
        (20, 11880.0, 396, 2376),
        (21, 19800.0, 792, 4752),
        (22, 20250.0, 1620, 8100),
        (30, 40500.0, 1620, 8100),
        (31, 108000.0, 3600, 18000),
        (32, 216000.0, 5120, 20480),
        (40, 245760.0, 8192, 32768),
        (42, 522240.0, 8704, 34816),
        (50, 589824.0, 22080, 110400),
        (51, 983040.0, 36864, 184320),
        (52, 2073600.0, 36864, 184320),
        (60, 4177920.0, 139264, 696320),
    ];
    let mbps = (w * h) as f64 * fps.0 as f64 / fps.1 as f64;
    for &(l, max_mbps, fs, dpb) in &T {
        if mbps <= max_mbps
            && w * h <= fs
            && w * w <= 8 * fs
            && h * h <= 8 * fs
            && num_ref.max(1) * w * h <= dpb
        {
            return l;
        }
    }
    60
}

// ---------------------------------------------------------------- layout del mdat

struct Frame {
    pos: usize, // offset del prefijo de longitud de 4 bytes
    len: usize,
    hdr: u8,
}

/// Busca la firma "options:" de x264 (dentro del SEI de datos de usuario del primer IDR) en vez de
/// asumir que el SEI es lo primero del mdat: el muxer puede intercalar un paquete de audio antes
/// (habitual por el "priming delay" del encoder AAC). Devuelve (offset del prefijo de 4 bytes, longitud del NAL).
fn find_x264_sei(d: &[u8], start: usize, end: usize) -> Option<(usize, usize)> {
    const NEEDLE: &[u8] = b"options:";
    let window = (start + 4_000_000).min(end);
    let text_at = d[start..window].windows(NEEDLE.len()).position(|w| w == NEEDLE)? + start;
    for q in (start.max(text_at.saturating_sub(300))..=text_at).rev() {
        if q + 8 > end || d[q] != 0 || d[q + 1] != 0 {
            continue;
        }
        let len = u32::from_be_bytes([d[q], d[q + 1], d[q + 2], d[q + 3]]) as usize;
        if len < 3 || len > MAX_NAL || q + 4 + len > end {
            continue;
        }
        if d[q + 4] & 0x1f == 6 && q + 4 + len > text_at {
            return Some((q, len));
        }
    }
    None
}

fn find_mdat(d: &[u8]) -> Option<usize> {
    let mut off = 0usize;
    while off + 8 <= d.len() {
        let size = u32::from_be_bytes(d[off..off + 4].try_into().ok()?) as usize;
        let typ = &d[off + 4..off + 8];
        let (hdr, size) = if size == 1 {
            if off + 16 > d.len() {
                return None;
            }
            (16, u64::from_be_bytes(d[off + 8..off + 16].try_into().ok()?) as usize)
        } else {
            (8, size)
        };
        if typ == b"mdat" {
            return Some(off + hdr);
        }
        if size < hdr {
            return None;
        }
        off += size;
    }
    None
}

/// Un slice de x264: longitud plausible, cabecera NAL 1/5 y slice header con first_mb=0,
/// slice_type 5..7 (x264 escribe tipo+5) y pps_id=0.
fn video_at(d: &[u8], q: usize, end: usize) -> Option<usize> {
    if q + 8 > end || d[q] != 0 {
        return None;
    }
    let len = u32::from_be_bytes([d[q], d[q + 1], d[q + 2], d[q + 3]]) as usize;
    if len < 3 || len > MAX_NAL || q + 4 + len > end {
        return None;
    }
    let h = d[q + 4];
    let t = h & 0x1f;
    if h & 0x80 != 0 || !(t == 1 || t == 5) || (t == 5 && h >> 5 == 0) {
        return None;
    }
    let mut r = Bits::new(&d[q + 5..q + 8]);
    if r.bit()? != 1 {
        return None;
    }
    let st = r.ue()?;
    if !(5..=7).contains(&st) || (t == 5 && st != 7) || r.ue()? != 0 {
        return None;
    }
    Some(len)
}

/// frame_num del slice (tras first_mb, slice_type y pps_id).
fn frame_num(d: &[u8], q: usize, len: usize, log2_fn: u32) -> Option<u32> {
    let raw = unescape(&d[q + 5..(q + 5 + 12).min(q + 4 + len)]);
    let mut r = Bits::new(&raw);
    r.ue()?;
    r.ue()?;
    r.ue()?;
    r.u(log2_fn)
}

/// Recorre el mdat separando frames de video (con su `frame_num` esperado: IDR=0, resto = ultimo
/// frame de referencia + 1) de los huecos de audio que hay entre ellos. El `frame_num` esperado ya
/// es un filtro muy estricto (junto con el slice header valido que exige `video_at`), asi que no
/// hace falta ademas adivinar con que byte "suele" empezar el audio — eso resulto fragil: el primer
/// paquete de audio que escribe ffmpeg a veces trae texto legible (identificacion del encoder) en
/// vez de datos binarios tipicos, y un chequeo por byte-mas-comun lo rechazaba como limite invalido.
fn scan(d: &[u8], start: usize, end: usize, log2_fn: u32, limit: usize) -> (Vec<Frame>, Vec<(usize, usize)>) {
    let modulo = 1u32 << log2_fn;
    let cand = |q: usize, last_ref: u32| -> Option<(usize, u32)> {
        let len = video_at(d, q, end)?;
        let fnum = frame_num(d, q, len, log2_fn)?;
        let idr = d[q + 4] & 0x1f == 5;
        let expected = if idr { 0 } else { (last_ref + 1) % modulo };
        (fnum == expected).then_some((len, fnum))
    };
    let mut frames = Vec::new();
    let mut gaps = Vec::new();
    let mut last_ref = 0u32;
    let mut q = start;
    while q < end && frames.len() < limit {
        if let Some((len, fnum)) = cand(q, last_ref) {
            if d[q + 4] >> 5 != 0 {
                last_ref = fnum;
            }
            frames.push(Frame { pos: q, len, hdr: d[q + 4] });
            q += 4 + len;
            continue;
        }
        let s = q;
        q += 1;
        while q < end && !(d[q] == 0 && cand(q, last_ref).is_some()) {
            q += 1;
        }
        gaps.push((s, q - s));
    }
    (frames, gaps)
}

fn annexb(d: &[u8], frames: &[Frame]) -> Vec<u8> {
    let mut o = Vec::new();
    for f in frames {
        o.extend(SC);
        o.extend(&d[f.pos + 4..f.pos + 4 + f.len]);
    }
    o
}

// ---------------------------------------------------------------- ffmpeg

/// Una geometria candidata equivocada puede hacer que ffmpeg gaste mucho tiempo "concealing" un
/// frame entero (o los 48 de `validates`) mal declarado, así que estas pruebas siempre corren con
/// limite de tiempo: pasado el plazo se mata el proceso y se trata como candidato invalido.
fn ffmpeg_io_timeout(args: &[&str], input: Vec<u8>, timeout: std::time::Duration) -> Result<(Vec<u8>, String), String> {
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("No se pudo iniciar ffmpeg: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("sin stdin")?;
    let mut stdout = child.stdout.take().ok_or("sin stdout")?;
    let mut stderr = child.stderr.take().ok_or("sin stderr")?;
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = writer.join();
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err("ffmpeg tardo demasiado (geometria descartada)".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            Err(e) => return Err(format!("ffmpeg: {e}")),
        }
    }
    let _ = writer.join();
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    Ok((stdout, stderr))
}

fn run_ffmpeg(args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-y"])
        .args(args)
        .output()
        .map_err(|e| format!("No se pudo iniciar ffmpeg: {e}"))
}

// ---------------------------------------------------------------- busqueda de geometria

struct Geo {
    depth: u32,
    qp: i32,
    w_mb: u32,
    h_mb: u32,
}

/// Decodifica solo el primer IDR con una altura enorme: si el ancho es correcto, ffmpeg no da error y el
/// numero de macrobloques "concealed" revela cuantas filas sobran (=> altura real).
fn test_width(x: &X264, p: &Params, depth: u32, qp: i32, w: u32, idr: &[u8]) -> Option<u32> {
    let mut data = Vec::new();
    data.extend(SC);
    data.extend(make_sps(p, depth, w, TEST_H_MB, 52, (0, 0), None));
    data.extend(SC);
    data.extend(make_pps(x, qp));
    data.extend(idr);
    let (_, err) = ffmpeg_io_timeout(
        &["-v", "info", "-f", "h264", "-i", "-", "-frames:v", "1", "-f", "null", "-"],
        data,
        std::time::Duration::from_secs(4),
    )
    .ok()?;
    if err.contains("error while decoding") || err.contains("Nothing was written") {
        return None;
    }
    let concealed = err
        .lines()
        .find_map(|l| l.split("concealing ").nth(1))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0);
    if concealed % w != 0 {
        return None;
    }
    let h = TEST_H_MB.checked_sub(concealed / w)?;
    (h > 0).then_some(h)
}

/// Diferencia absoluta media entre muestras de frames consecutivos, normalizada a [0,1] segun la
/// profundidad (para poder comparar 8 y 10 bit entre si). Un video real tiene movimiento suave; una
/// profundidad de color equivocada desfasa la escala de cuantizacion y genera prediccion inter mala,
/// que se nota como ruido/temblor frame a frame mucho mayor que el de un video genuino.
fn temporal_roughness(out: &[u8], frame_bytes: u64, out_frames: u64, depth: u32) -> f64 {
    if out_frames < 2 || frame_bytes == 0 {
        return f64::MAX;
    }
    let fb = frame_bytes as usize;
    let step = if depth > 8 { 194 } else { 97 }; // primo, para no pisar siempre el mismo plano/muestra
    let mut total = 0f64;
    let mut n = 0u64;
    for i in 1..out_frames as usize {
        let a = &out[(i - 1) * fb..i * fb];
        let b = &out[i * fb..(i + 1) * fb];
        let mut j = 0;
        while j + 1 < fb {
            let (va, vb) = if depth > 8 {
                (u16::from_le_bytes([a[j], a[j + 1]]) as f64, u16::from_le_bytes([b[j], b[j + 1]]) as f64)
            } else {
                (a[j] as f64, b[j] as f64)
            };
            total += (va - vb).abs();
            n += 1;
            j += step;
        }
    }
    let max = ((1u32 << depth) - 1) as f64;
    total / n.max(1) as f64 / max
}

/// `Some(aspereza_temporal)` si la geometria/profundidad/QP candidata decodifica limpio, `None` si no.
fn validates(x: &X264, p: &Params, g: &Geo, d: &[u8], frames: &[Frame]) -> Option<f64> {
    let mut data = Vec::new();
    data.extend(SC);
    data.extend(make_sps(p, g.depth, g.w_mb, g.h_mb, 52, (0, 0), None));
    data.extend(SC);
    data.extend(make_pps(x, g.qp));
    let n = frames.len().min(48);
    data.extend(annexb(d, &frames[..n]));
    // Un ancho/alto demasiado chico puede "decodificar sin error": el decoder simplemente da por
    // completo el frame declarado y descarta el resto de esa NAL sin quejarse, y como cada frame
    // real empieza con su propio startcode, el siguiente si arranca limpio — 48 frames "sin error"
    // pero con una imagen absurdamente pequeña. Pedir video crudo (`rawvideo`) y verificar que salio
    // el numero exacto de bytes esperado (ancho×alto×frames, segun bit-depth) descarta ese falso
    // positivo sin tener que decodificar realmente los pixeles.
    let bytes_per_sample = if g.depth > 8 { 2 } else { 1 };
    let frame_bytes = (g.w_mb as u64 * 16) * (g.h_mb as u64 * 16) * 3 / 2 * bytes_per_sample;
    let pix_fmt = if g.depth > 8 { "yuv420p10le" } else { "yuv420p" };
    match ffmpeg_io_timeout(
        &["-v", "error", "-f", "h264", "-i", "-", "-pix_fmt", pix_fmt, "-f", "rawvideo", "-"],
        data,
        std::time::Duration::from_secs(8),
    ) {
        Ok((out, err)) => {
            // El decoder retiene unos pocos frames en su buffer de reordenamiento B (hasta el "delay"
            // de la piramide) que nunca salen si el stream de prueba termina ahi mismo — un short-fall
            // chico es normal, no señal de geometria incorrecta. Se exige salida limpia, un numero
            // ENTERO de frames a este tamano (nunca un frame a medias) y no mas de 8 de menos.
            let out_frames = out.len() as u64 / frame_bytes.max(1);
            let ok = err.trim().is_empty()
                && frame_bytes > 0
                && out.len() as u64 % frame_bytes == 0
                && out_frames <= n as u64
                && out_frames + 8 >= n as u64;
            let roughness = ok.then(|| temporal_roughness(&out, frame_bytes, out_frames, g.depth));
            if std::env::var_os("RECOVER_DEBUG").is_some() {
                eprintln!(
                    "    validates depth={} qp={} num_ref={} {}x{} -> {ok} frames={}/{} roughness={:?} {:?}",
                    g.depth,
                    g.qp,
                    p.num_ref,
                    g.w_mb,
                    g.h_mb,
                    out_frames,
                    n,
                    roughness,
                    &err[..err.len().min(1200)]
                );
            }
            roughness
        }
        Err(e) => {
            if std::env::var_os("RECOVER_DEBUG").is_some() {
                eprintln!("    validates depth={} qp={} {}x{} -> timeout/err {e}", g.depth, g.qp, g.w_mb, g.h_mb);
            }
            None
        }
    }
}

/// Extrae el "error while decoding MB x y" que reporta ffmpeg, si lo hay.
fn first_error_mb(err: &str) -> Option<u32> {
    let rest = err.split("error while decoding MB ").nth(1)?;
    rest.split_whitespace().next()?.parse().ok()
}

/// Detecta el QP inicial sin necesidad de conocer el ancho real: se declara una fila ficticia de
/// `MAX_W_MB` macrobloques (mucho mas ancha que cualquier video real) y se mira en cual macrobloque
/// revienta la decodificacion del primer IDR. Con el QP correcto, el contexto CABAC arranca bien y la
/// decodificacion avanza limpio hasta donde el ancho real hace que el bitstream empiece a alimentar
/// datos de la fila 1 (revienta lejos, o ni siquiera revienta si el video es mas ancho que la fila
/// ficticia); con un QP equivocado, el contexto arranca mal y revienta de inmediato en el macrobloque 0.
/// Esto permite buscar el QP (0..=51) sin combinarlo con el ancho, evitando una busqueda combinada
/// carisima: el QP inicial que x264 escribe en la PPS no es un valor fijo (varia con CRF, ABR,
/// lookahead, AQ...) y adivinarlo con una lista corta de candidatos no es confiable en general.
fn probe_qp(x: &X264, p: &Params, depth: u32, qp: i32, idr: &[u8]) -> Option<u32> {
    let mut data = Vec::new();
    data.extend(SC);
    data.extend(make_sps(p, depth, MAX_W_MB, 1, 52, (0, 0), None));
    data.extend(SC);
    data.extend(make_pps(x, qp));
    data.extend(idr);
    let (_, err) = ffmpeg_io_timeout(&["-v", "error", "-f", "h264", "-i", "-", "-frames:v", "1", "-f", "null", "-"], data, std::time::Duration::from_secs(3)).ok()?;
    if err.trim().is_empty() {
        return Some(u32::MAX); // ni siquiera reventó: el video es mas ancho que la fila de prueba
    }
    first_error_mb(&err)
}

fn find_geometry(x: &X264, d: &[u8], frames: &[Frame]) -> Result<(Geo, Params), String> {
    let idr = annexb(d, &frames[..1]);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let width_search = |depth: u32, p: &Params, qp: i32| -> Option<(Geo, Params, f64)> {
        let next = AtomicUsize::new(1);
        let found = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| loop {
                    let w = next.fetch_add(1, Ordering::Relaxed) as u32;
                    if w > MAX_W_MB {
                        break;
                    }
                    if let Some(h) = test_width(x, p, depth, qp, w, &idr) {
                        found.lock().unwrap().push((w, h));
                    }
                });
            }
        });
        let mut cands = found.into_inner().unwrap();
        cands.sort();
        if std::env::var_os("RECOVER_DEBUG").is_some() {
            eprintln!("  qp={qp} width candidates={:?}", &cands[..cands.len().min(10)]);
        }
        // El ancho/alto no dependen de num_ref (el IDR de prueba no tiene listas de referencia), pero
        // "cuantas referencias declara la SPS" si puede quedarse corto: la formula de x264 es solo un
        // punto de partida, asi que si el candidato no valida se prueba con mas espacio de referencias
        // (hasta el maximo de la norma) antes de descartarlo.
        let mut num_refs = vec![p.num_ref, 8, 16];
        num_refs.dedup();
        // Un ancho MUCHO mas chico que el real tambien puede "validar" sin error: cada frame real
        // arranca en su propio startcode, asi que el decoder simplemente da por completo el frame
        // diminuto que declaramos y descarta en silencio el resto de esa NAL, repitiendolo limpio
        // frame tras frame. No hay forma de detectar eso mirando un solo candidato — hay que evaluar
        // TODOS y quedarse con el de mayor area que valide (el genuino sigue validando igual, y es
        // extremadamente improbable que un tamano de verdad mas grande tambien "valide" por accidente).
        let mut best: Option<(u64, Geo, Params, f64)> = None;
        for (w_mb, h_mb) in cands {
            let g = Geo { depth, qp, w_mb, h_mb };
            for &num_ref in &num_refs {
                let mut p2 = p.clone();
                p2.num_ref = num_ref;
                if let Some(roughness) = validates(x, &p2, &g, d, frames) {
                    let area = w_mb as u64 * h_mb as u64;
                    if best.as_ref().is_none_or(|(a, _, _, _)| area > *a) {
                        best = Some((area, g, p2, roughness));
                    }
                    break; // ya valido con este num_ref, no hace falta probar los mas grandes
                }
            }
        }
        best.map(|(_, g, p2, roughness)| (g, p2, roughness))
    };
    // La sintaxis CABAC no cambia con la profundidad de color (solo la INTERPRETACION de los
    // coeficientes): un video de 10 bit puede "validar" (decodificar sin error, del tamano exacto
    // esperado) igual de limpio si se declara como 8 bit, y viceversa. Por eso se prueban SIEMPRE
    // las dos profundidades — no alcanza con parar en la primera que valide — y si ambas lo hacen,
    // se desempata con `temporal_roughness` (ya calculada gratis dentro de `validates`, reutilizando
    // los mismos 48 frames decodificados): la profundidad equivocada desfasa la escala de
    // cuantizacion y arruina la prediccion inter entre frames, lo que se nota como temblor/ruido
    // frame a frame mucho mayor que el de un video real con movimiento suave.
    let mut found: Vec<(u32, Geo, Params, f64)> = Vec::new();
    for depth in [8u32, 10] {
        let p = derive(x, depth);
        // Candidatos de QP ordenados por que tan lejos llego la decodificacion (mejor primero); un
        // "OK" total (u32::MAX) o un fallo muy temprano se descartan igual, se prueban en orden.
        let next = AtomicUsize::new(0);
        let scored = Mutex::new(Vec::new());
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| loop {
                    let qp = next.fetch_add(1, Ordering::Relaxed) as i32;
                    if qp > 51 {
                        break;
                    }
                    if let Some(mbx) = probe_qp(x, &p, depth, qp, &idr) {
                        scored.lock().unwrap().push((mbx, qp));
                    }
                });
            }
        });
        let mut cands = scored.into_inner().unwrap();
        cands.sort_by(|a, b| b.cmp(a));
        if std::env::var_os("RECOVER_DEBUG").is_some() {
            eprintln!("depth={depth} top qp candidates={:?}", &cands[..cands.len().min(10)]);
        }
        for (_, qp) in cands.into_iter().take(5) {
            if let Some((g, params, roughness)) = width_search(depth, &p, qp) {
                found.push((depth, g, params, roughness));
                break;
            }
        }
    }
    if std::env::var_os("RECOVER_DEBUG").is_some() && found.len() > 1 {
        eprintln!(
            "ambas profundidades validaron: {:?}",
            found.iter().map(|(d, _, _, r)| (d, r)).collect::<Vec<_>>()
        );
    }
    found
        .into_iter()
        .min_by(|a, b| a.3.total_cmp(&b.3))
        .map(|(_, g, params, _)| (g, params))
        .ok_or_else(|| "No se pudo deducir el tamano ni la profundidad de color del video.".into())
}

// ---------------------------------------------------------------- recorte (relleno de x264)

fn trailing_pad(v: &[f64]) -> usize {
    let n = v.len();
    if n < 64 {
        return 0;
    }
    // media global del eje: un fondo liso cerca del borde no debe confundirse con relleno
    let base = v[1..].iter().sum::<f64>() / (n - 1) as f64;
    if base < 1.0 {
        return 0; // imagen demasiado plana para decidir
    }
    // El relleno de x264 replica el ultimo pixel real: la diferencia entre columnas/filas de relleno
    // deberia ser practicamente cero, no solo "menor que el promedio" (un fondo liso real tambien baja
    // el promedio local, pero rara vez llega a un valor absoluto tan chico).
    let mut k = 0;
    while k < 15 && v[n - 1 - k] < 0.35 * base && v[n - 1 - k] < 0.6 {
        k += 1;
    }
    k - k % 2 // el recorte se expresa en unidades de 2 px
}

/// x264 rellena el borde derecho/inferior replicando el ultimo pixel: esas columnas/filas casi no
/// cambian respecto de la vecina. Devuelve (pad_derecho, pad_inferior) en pixeles.
fn detect_pad(
    x: &X264,
    p: &Params,
    g: &Geo,
    d: &[u8],
    frames: &[Frame],
) -> (usize, usize) {
    let (w, h) = ((g.w_mb * 16) as usize, (g.h_mb * 16) as usize);
    let idrs: Vec<&Frame> = frames.iter().filter(|f| f.hdr & 0x1f == 5).collect();
    if idrs.is_empty() {
        return (0, 0);
    }
    let picks = idrs.len().min(5);
    let mut cols = vec![0f64; w];
    let mut rows = vec![0f64; h];
    let mut used = 0;
    for i in 0..picks {
        let f = idrs[i * idrs.len() / picks];
        let mut data = Vec::new();
        data.extend(SC);
        data.extend(make_sps(p, g.depth, g.w_mb, g.h_mb, 52, (0, 0), None));
        data.extend(SC);
        data.extend(make_pps(x, g.qp));
        data.extend(SC);
        data.extend(&d[f.pos + 4..f.pos + 4 + f.len]);
        let Ok((raw, _)) = ffmpeg_io_timeout(
            &["-v", "error", "-f", "h264", "-i", "-", "-frames:v", "1", "-vf", "format=gray", "-f", "rawvideo", "-"],
            data,
            std::time::Duration::from_secs(5),
        ) else {
            continue;
        };
        if raw.len() != w * h {
            continue;
        }
        used += 1;
        for y in (0..h).step_by(2) {
            for xx in 1..w {
                cols[xx] += (raw[y * w + xx] as f64 - raw[y * w + xx - 1] as f64).abs();
            }
        }
        for y in 1..h {
            for xx in (0..w).step_by(2) {
                rows[y] += (raw[y * w + xx] as f64 - raw[(y - 1) * w + xx] as f64).abs();
            }
        }
    }
    if used == 0 {
        return (0, 0);
    }
    if std::env::var_os("RECOVER_DEBUG").is_some() {
        eprintln!("  pad cols tail={:?}", &cols[cols.len() - 20..]);
        eprintln!("  pad rows tail={:?}", &rows[rows.len() - 20..]);
    }
    (trailing_pad(&cols), trailing_pad(&rows))
}

// ---------------------------------------------------------------- MP4

fn bx(t: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let n: usize = parts.iter().map(|p| p.len()).sum();
    let mut o = Vec::with_capacity(8 + n);
    o.extend(((8 + n) as u32).to_be_bytes());
    o.extend(t);
    for p in parts {
        o.extend(*p);
    }
    o
}

fn full(t: &[u8; 4], version: u8, flags: u32, parts: &[&[u8]]) -> Vec<u8> {
    let head = ((version as u32) << 24 | flags).to_be_bytes();
    let mut all: Vec<&[u8]> = vec![&head];
    all.extend(parts);
    bx(t, &all)
}

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

const MATRIX: [u32; 9] = [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn mvhd(dur_ms: u32) -> Vec<u8> {
    let mut b = Vec::new();
    for v in [0, 0, 1000, dur_ms] {
        b.extend(be32(v));
    }
    b.extend([0, 1, 0, 0, 1, 0, 0, 0]); // rate 1.0, volume 1.0, reserved
    b.extend([0u8; 8]);
    b.extend(matrix());
    b.extend([0u8; 24]);
    b.extend(be32(2));
    full(b"mvhd", 0, 0, &[&b])
}

fn tkhd(dur_ms: u32, w: u32, h: u32, audio: bool) -> Vec<u8> {
    let mut b = Vec::new();
    for v in [0, 0, 1, 0, dur_ms] {
        b.extend(be32(v));
    }
    b.extend([0u8; 8]);
    b.extend([0, 0, 0, 0]); // layer, alternate_group
    b.extend(if audio { [1, 0] } else { [0, 0] }); // volume
    b.extend([0, 0]);
    b.extend(matrix());
    b.extend(be32(w << 16));
    b.extend(be32(h << 16));
    full(b"tkhd", 0, 3, &[&b])
}

fn mdhd(ts: u32, dur: u32) -> Vec<u8> {
    let mut b = Vec::new();
    for v in [0, 0, ts, dur] {
        b.extend(be32(v));
    }
    b.extend([0x55, 0xc4, 0, 0]);
    full(b"mdhd", 0, 0, &[&b])
}

fn hdlr(kind: &[u8; 4], name: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 4];
    b.extend(kind);
    b.extend([0u8; 12]);
    b.extend(name);
    full(b"hdlr", 0, 0, &[&b])
}

fn dinf() -> Vec<u8> {
    let url = full(b"url ", 0, 1, &[]);
    bx(b"dinf", &[&full(b"dref", 0, 0, &[&be32(1), &url])])
}

fn ftyp() -> Vec<u8> {
    bx(b"ftyp", &[b"isom", &be32(512), b"isomiso2avc1mp41"])
}

fn tmp_write(path: &Path) -> Result<BufWriter<File>, String> {
    File::create(path)
        .map(|f| BufWriter::with_capacity(1 << 20, f))
        .map_err(|e| format!("No se pudo escribir {}: {e}", path.display()))
}

/// slice header -> orden de presentacion de cada frame (indice de display).
fn display_order(d: &[u8], frames: &[Frame], p: &Params) -> Result<Vec<usize>, String> {
    let n = frames.len();
    if p.poc_type != 0 {
        return Ok((0..n).collect());
    }
    let max_lsb = 1i64 << p.log2_poc;
    let (mut prev_msb, mut prev_lsb) = (0i64, 0i64);
    let mut pocs = Vec::with_capacity(n);
    let mut idr = Vec::with_capacity(n);
    for f in frames {
        let end = (f.pos + 5 + 24).min(f.pos + 4 + f.len);
        let raw = unescape(&d[f.pos + 5..end]);
        let mut r = Bits::new(&raw);
        let is_idr = f.hdr & 0x1f == 5;
        let lsb = (|| {
            r.ue()?;
            r.ue()?;
            r.ue()?;
            r.u(p.log2_fn)?;
            if is_idr {
                r.ue()?;
            }
            r.u(p.log2_poc)
        })()
        .ok_or("No se pudo leer el orden de los frames (slice header).")? as i64;
        if is_idr {
            prev_msb = 0;
            prev_lsb = 0;
        }
        let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
            prev_msb + max_lsb
        } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
            prev_msb - max_lsb
        } else {
            prev_msb
        };
        pocs.push(msb + lsb);
        idr.push(is_idr);
        if f.hdr >> 5 != 0 {
            prev_msb = msb;
            prev_lsb = lsb;
        }
    }
    let mut disp = vec![0usize; n];
    let (mut base, mut i) = (0usize, 0usize);
    while i < n {
        let mut j = i + 1;
        while j < n && !idr[j] {
            j += 1;
        }
        let mut order: Vec<usize> = (i..j).collect();
        order.sort_by_key(|&k| pocs[k]);
        for (rank, k) in order.into_iter().enumerate() {
            disp[k] = base + rank;
        }
        base += j - i;
        i = j;
    }
    Ok(disp)
}

struct VideoOut<'a> {
    sps: &'a [u8],
    pps: &'a [u8],
    depth: u32,
    width: u32,
    height: u32,
    fps: (u32, u32),
}

fn write_video_mp4(path: &Path, d: &[u8], frames: &[Frame], p: &Params, v: &VideoOut) -> Result<(), String> {
    let n = frames.len();
    let disp = display_order(d, frames, p)?;
    let delay = (0..n).map(|i| i as i64 - disp[i] as i64).max().unwrap_or(0).max(0) as u32;
    let (ts, dur) = v.fps;

    let ranges: Vec<(usize, usize)> = frames.iter().map(|f| (f.pos, f.pos + 4 + f.len)).collect();
    let payload: u64 = ranges.iter().map(|(a, b)| (b - a) as u64).sum();
    let big = payload + 8 > 0xFFFF_FFF0;
    let ftyp = ftyp();
    let mdat_hdr_len = if big { 16 } else { 8 };
    let offset = ftyp.len() as u64 + mdat_hdr_len;

    let mut avcc = vec![1, v.sps[1], v.sps[2], v.sps[3], 0xFF, 0xE1];
    avcc.extend((v.sps.len() as u16).to_be_bytes());
    avcc.extend(v.sps);
    avcc.push(1);
    avcc.extend((v.pps.len() as u16).to_be_bytes());
    avcc.extend(v.pps);
    if matches!(v.sps[1], 100 | 110 | 122 | 244) {
        avcc.extend([0xFC | 1, 0xF8 | (v.depth - 8) as u8, 0xF8 | (v.depth - 8) as u8, 0]);
    }
    let mut avc1 = vec![0u8; 6];
    avc1.extend(1u16.to_be_bytes());
    avc1.extend([0u8; 16]);
    avc1.extend((v.width as u16).to_be_bytes());
    avc1.extend((v.height as u16).to_be_bytes());
    avc1.extend(0x480000u32.to_be_bytes());
    avc1.extend(0x480000u32.to_be_bytes());
    avc1.extend([0u8; 4]);
    avc1.extend(1u16.to_be_bytes());
    avc1.extend([0u8; 32]);
    avc1.extend(0x18u16.to_be_bytes());
    avc1.extend((-1i16).to_be_bytes());
    avc1.extend(bx(b"avcC", &[&avcc]));
    let stsd = full(b"stsd", 0, 0, &[&be32(1), &bx(b"avc1", &[&avc1])]);

    let stts = full(b"stts", 0, 0, &[&be32(1), &be32(n as u32), &be32(dur)]);
    let mut stbl_parts: Vec<Vec<u8>> = vec![stsd, stts];
    if delay > 0 {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for i in 0..n {
            let c = (disp[i] as u32 + delay) - i as u32;
            match runs.last_mut() {
                Some(r) if r.1 == c => r.0 += 1,
                _ => runs.push((1, c)),
            }
        }
        let mut b = be32(runs.len() as u32).to_vec();
        for (cnt, off) in runs {
            b.extend(be32(cnt));
            b.extend(be32(off * dur));
        }
        stbl_parts.push(full(b"ctts", 0, 0, &[&b]));
    }
    let keys: Vec<u32> = (0..n).filter(|&i| frames[i].hdr & 0x1f == 5).map(|i| i as u32 + 1).collect();
    let mut b = be32(keys.len() as u32).to_vec();
    for k in keys {
        b.extend(be32(k));
    }
    stbl_parts.push(full(b"stss", 0, 0, &[&b]));
    stbl_parts.push(full(b"stsc", 0, 0, &[&be32(1), &be32(1), &be32(n as u32), &be32(1)]));
    let mut b = be32(0).to_vec();
    b.extend(be32(n as u32));
    for (a, e) in &ranges {
        b.extend(be32((e - a) as u32));
    }
    stbl_parts.push(full(b"stsz", 0, 0, &[&b]));
    if big {
        stbl_parts.push(full(b"co64", 0, 0, &[&be32(1), &offset.to_be_bytes()]));
    } else {
        stbl_parts.push(full(b"stco", 0, 0, &[&be32(1), &be32(offset as u32)]));
    }
    let stbl = bx(b"stbl", &stbl_parts.iter().map(|v| v.as_slice()).collect::<Vec<_>>());

    let minf = bx(b"minf", &[&full(b"vmhd", 0, 1, &[&[0u8; 8]]), &dinf(), &stbl]);
    let mdia = bx(b"mdia", &[&mdhd(ts, n as u32 * dur), &hdlr(b"vide", b"v\0"), &minf]);
    let pres_ms = ((n as u64 - delay as u64) * dur as u64 * 1000 / ts as u64) as u32;
    let mut elst_b = be32(1).to_vec();
    elst_b.extend(be32(pres_ms));
    elst_b.extend(be32(delay * dur));
    elst_b.extend([0, 1, 0, 0]);
    let edts = bx(b"edts", &[&full(b"elst", 0, 0, &[&elst_b])]);
    let trak = bx(b"trak", &[&tkhd(pres_ms, v.width, v.height, false), &edts, &mdia]);
    let moov = bx(b"moov", &[&mvhd(pres_ms), &trak]);

    let mut w = tmp_write(path)?;
    let io = |e: std::io::Error| format!("Error escribiendo el video temporal: {e}");
    w.write_all(&ftyp).map_err(io)?;
    if big {
        w.write_all(&1u32.to_be_bytes()).map_err(io)?;
        w.write_all(b"mdat").map_err(io)?;
        w.write_all(&(payload + 16).to_be_bytes()).map_err(io)?;
    } else {
        w.write_all(&((payload + 8) as u32).to_be_bytes()).map_err(io)?;
        w.write_all(b"mdat").map_err(io)?;
    }
    for (a, e) in &ranges {
        w.write_all(&d[*a..*e]).map_err(io)?;
    }
    w.write_all(&moov).map_err(io)?;
    w.flush().map_err(io)
}

// ---------------------------------------------------------------- audio

fn aac_wrapper(path: &Path, d: &[u8], gaps: &[(usize, usize)], rate: u32, ch: u32) -> Result<(), String> {
    const RATES: [u32; 12] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000];
    let idx = RATES.iter().position(|&r| r == rate).ok_or("frecuencia de audio invalida")? as u16;
    let asc = ((2u16 << 11) | (idx << 7) | ((ch as u16) << 3)).to_be_bytes();
    let total: usize = gaps.iter().map(|g| g.1).sum();
    if total as u64 + 8 > 0xFFFF_0000 {
        return Err("Audio demasiado grande para reconstruir.".into());
    }
    let dsi = [&[0x05, asc.len() as u8][..], &asc].concat();
    let dec = [&[0x04, (13 + dsi.len()) as u8, 0x40, 0x15][..], &[0u8; 11], &dsi].concat();
    let es = [&[0x03, (3 + dec.len() + 3) as u8, 0, 0, 0][..], &dec, &[0x06, 0x01, 0x02]].concat();
    let esds = full(b"esds", 0, 0, &[&es]);
    let mut mp4a = vec![0u8; 6];
    mp4a.extend(1u16.to_be_bytes());
    mp4a.extend([0u8; 8]);
    mp4a.extend((ch as u16).to_be_bytes());
    mp4a.extend(16u16.to_be_bytes());
    mp4a.extend([0u8; 4]);
    mp4a.extend((rate << 16).to_be_bytes());
    mp4a.extend(&esds);
    let ftyp = ftyp();
    let offset = ftyp.len() as u32 + 8;
    let dur = 1_000_000u32; // nominal: ffmpeg decodifica todo el paquete igual
    let stbl = bx(
        b"stbl",
        &[
            &full(b"stsd", 0, 0, &[&be32(1), &bx(b"mp4a", &[&mp4a])]),
            &full(b"stts", 0, 0, &[&be32(1), &be32(1), &be32(dur)]),
            &full(b"stsc", 0, 0, &[&be32(1), &be32(1), &be32(1), &be32(1)]),
            &full(b"stsz", 0, 0, &[&be32(total as u32), &be32(1)]),
            &full(b"stco", 0, 0, &[&be32(1), &be32(offset)]),
        ],
    );
    let minf = bx(b"minf", &[&full(b"smhd", 0, 0, &[&[0u8; 4]]), &dinf(), &stbl]);
    let mdia = bx(b"mdia", &[&mdhd(rate, dur), &hdlr(b"soun", b"a\0"), &minf]);
    let ms = (dur as u64 * 1000 / rate as u64) as u32;
    let moov = bx(b"moov", &[&mvhd(ms), &bx(b"trak", &[&tkhd(ms, 0, 0, true), &mdia])]);

    let mut w = tmp_write(path)?;
    let io = |e: std::io::Error| format!("Error escribiendo el audio temporal: {e}");
    w.write_all(&ftyp).map_err(io)?;
    w.write_all(&((total + 8) as u32).to_be_bytes()).map_err(io)?;
    w.write_all(b"mdat").map_err(io)?;
    for (s, l) in gaps {
        w.write_all(&d[*s..*s + *l]).map_err(io)?;
    }
    w.write_all(&moov).map_err(io)?;
    w.flush().map_err(io)
}

/// Decodifica el audio crudo, lo recodifica a AAC y devuelve su duracion en segundos.
fn transcode_audio(wrapper: &Path, out: &Path) -> Result<f64, String> {
    let o = run_ffmpeg(&[
        "-v",
        "error",
        "-i",
        &wrapper.to_string_lossy(),
        "-c:a",
        "aac",
        "-b:a",
        "192k",
        "-progress",
        "pipe:1",
        "-nostats",
        &out.to_string_lossy(),
    ])?;
    // El ultimo frame de audio viene cortado: ffmpeg avisa pero conserva lo anterior.
    let secs = String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("out_time_us="))
        .filter_map(|v| v.parse::<f64>().ok())
        .last()
        .map(|us| us / 1e6)
        .unwrap_or(0.0);
    if secs <= 0.0 || fs::metadata(out).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err("No se pudo decodificar el audio del archivo.".into());
    }
    Ok(secs)
}

// ---------------------------------------------------------------- principal

struct TmpDir(PathBuf);

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn rebuild(input: &Path, output: &Path, report: Report) -> Result<Recovered, String> {
    let file = File::open(input).map_err(|e| format!("No se pudo abrir el archivo: {e}"))?;
    let mmap = unsafe { Mmap::map(&file) }.map_err(|e| format!("No se pudo leer el archivo: {e}"))?;
    let d: &[u8] = &mmap;

    report(2.0, "Analizando");
    let start = find_mdat(d).ok_or("El archivo no tiene datos de video (mdat).")?;
    let mut end = d.len();
    while end > start && d[end - 1] == 0 {
        end -= 1; // hueco de ceros de la descarga incompleta
    }
    if end < start + 16 {
        return Err("El archivo esta vacio o sin datos utiles.".into());
    }
    // El SEI de opciones de x264 no siempre es lo primero del mdat: si el muxer intercalo audio
    // antes del primer frame de video (comun con el "priming delay" del encoder AAC), hay que
    // buscarlo en vez de asumir que empieza en el primer byte.
    let (sei_start, sei_len) = find_x264_sei(d, start, end)
        .ok_or("Solo se pueden reconstruir videos codificados con x264 (no se encontro su firma).")?;
    let x = parse_x264(&d[sei_start + 5..sei_start + 4 + sei_len])?;
    let after_sei = sei_start + 4 + sei_len;

    let log2_fn = derive(&x, 8).log2_fn;
    let (frames, mut gaps) = scan(d, after_sei, end, log2_fn, usize::MAX);
    if frames.len() < 2 || frames[0].hdr & 0x1f != 5 {
        return Err("No se encontraron frames de video recuperables.".into());
    }
    if sei_start > start {
        // Lo que haya antes del SEI (tipicamente el priming del encoder de audio) es audio, no video.
        gaps.insert(0, (start, sei_start - start));
    }
    let n = frames.len();
    if std::env::var_os("RECOVER_DEBUG").is_some() {
        let dump: Vec<(usize, u8)> = frames[..n.min(48)].iter().map(|f| (f.pos, f.hdr)).collect();
        eprintln!("first 48 frames (pos, hdr byte): {dump:?}");
    }

    report(12.0, "Buscando parametros del video");
    let (geo, params) = find_geometry(&x, d, &frames)?;
    let (w_full, h_full) = (geo.w_mb * 16, geo.h_mb * 16);

    report(35.0, "Ajustando bordes");
    let (pad_r, pad_b) = detect_pad(&x, &params, &geo, d, &frames);
    let (w_disp, h_disp) = (w_full - pad_r as u32, h_full - pad_b as u32);

    // 2) audio: se decodifica junto y de ahi sale la duracion para deducir los fps
    let tmp = TmpDir(
        output
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!(
                ".reparador-tmp-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            )),
    );
    fs::create_dir_all(&tmp.0).map_err(|e| format!("No se pudo crear carpeta temporal: {e}"))?;
    let audio_bytes: usize = gaps.iter().map(|g| g.1).sum();
    // El primer paquete de audio a veces trae un prefijo de identificacion del encoder en vez del
    // byte tipico de inicio de un raw_data_block AAC (ver find_x264_sei, mismo fenomeno) — se usa la
    // clase de canal mas votada entre TODOS los huecos, no la del primero, para no dejarse enganar
    // por ese caso especial.
    let mut class_votes = [0usize; 8];
    for g in gaps.iter().filter(|g| g.1 > 0) {
        class_votes[(d[g.0] >> 5) as usize] += 1;
    }
    let total_votes: usize = class_votes.iter().sum();
    let channel_class = class_votes.iter().enumerate().max_by_key(|(_, &c)| c).map(|(i, _)| i);
    let has_audio = audio_bytes > 4096
        && total_votes > 0
        && matches!(channel_class, Some(0) | Some(1))
        && class_votes[channel_class.unwrap_or(0)] * 2 >= total_votes;
    let audio_m4a = tmp.0.join("audio.m4a");
    let mut fps = (30000, 1001);
    let mut audio_note = "sin audio";
    if has_audio {
        report(45.0, "Recuperando audio");
        let ch = if channel_class == Some(0) { 1 } else { 2 };
        let wrapper = tmp.0.join("audio_raw.mp4");
        aac_wrapper(&wrapper, d, &gaps, 48000, ch)?;
        let secs48 = transcode_audio(&wrapper, &audio_m4a)?;
        // 44.1 y 48 kHz se parsean igual: se elige la combinacion (rate, fps) que cuadra con el video
        let mut best = (f64::MAX, 48000u32, fps);
        for rate in [48000u32, 44100] {
            let audio_secs = secs48 * 48000.0 / rate as f64;
            for f in STD_FPS {
                let vs = n as f64 * f.1 as f64 / f.0 as f64;
                let err = (vs - audio_secs).abs() / audio_secs;
                if err < best.0 {
                    best = (err, rate, f);
                }
            }
        }
        fps = best.2;
        if best.1 != 48000 {
            aac_wrapper(&wrapper, d, &gaps, best.1, ch)?;
            transcode_audio(&wrapper, &audio_m4a)?;
        }
        audio_note = "audio recodificado a AAC";
    }

    // 3) video-only con indice correcto, y mux final
    report(70.0, "Reconstruyendo video");
    let level = pick_level(geo.w_mb, geo.h_mb, params.num_ref, fps);
    let crop = ((pad_r / 2) as u32, (pad_b / 2) as u32);
    let sps = make_sps(&params, geo.depth, geo.w_mb, geo.h_mb, level, crop, Some(fps));
    let pps = make_pps(&x, geo.qp);
    let video_mp4 = tmp.0.join("video.mp4");
    write_video_mp4(
        &video_mp4,
        d,
        &frames,
        &params,
        &VideoOut { sps: &sps, pps: &pps, depth: geo.depth, width: w_disp, height: h_disp, fps },
    )?;

    report(88.0, "Uniendo audio y video");
    let out_s = output.to_string_lossy().to_string();
    let vs = video_mp4.to_string_lossy().to_string();
    let as_ = audio_m4a.to_string_lossy().to_string();
    let mut args = vec!["-v", "error", "-i", &vs];
    if has_audio {
        args.extend(["-i", &as_, "-map", "0:v:0", "-map", "1:a:0"]);
    }
    args.extend(["-c", "copy", "-movflags", "+faststart", &out_s]);
    let o = run_ffmpeg(&args)?;
    if !o.status.success() || fs::metadata(output).map(|m| m.len()).unwrap_or(0) == 0 {
        return Err(format!(
            "No se pudo armar el MP4 final: {}",
            String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("")
        ));
    }

    report(100.0, "Listo");
    Ok(Recovered {
        note: format!(
            "Reconstruido desde cero (faltaba el indice): {} frames, {}x{} a {:.3} fps, {}.",
            n,
            w_disp,
            h_disp,
            fps.0 as f64 / fps.1 as f64,
            audio_note
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RECOVER_TEST_INPUT=/ruta/video.mp4 cargo test --offline real_file -- --ignored --nocapture
    #[test]
    #[ignore]
    fn real_file() {
        let input = std::env::var("RECOVER_TEST_INPUT").expect("RECOVER_TEST_INPUT");
        let out = std::env::temp_dir().join("recover_test_out.mp4");
        let r = rebuild(Path::new(&input), &out, &|p, s| println!("{p:5.1}% {s}")).unwrap();
        println!("{}", r.note);
    }

    #[test]
    fn pps_matches_x264() {
        let x = X264 {
            cabac: true,
            bframes: 3,
            pyramid: 2,
            refs: 1,
            t8x8: true,
            weightp: 1,
            weightb: true,
            cqo: 0,
            constrained: false,
            keyint: 250,
        };
        assert_eq!(make_pps(&x, 23), [0x68, 0xef, 0x8f, 0xcb]);
        let p = derive(&x, 10);
        assert_eq!((p.num_ref, p.log2_fn, p.poc_type, p.log2_poc, p.profile), (4, 4, 0, 6, 110));
    }
}
