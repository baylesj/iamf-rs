#!/usr/bin/env python3
"""Extracts rendering gain matrices from libiamf v1.1 m2m_rdr.c / h2m_rdr.c
into a generated Rust module for iamf-dec.

Usage: extract_matrices.py <libiamf-checkout>/code/src/iamf_dec
(pin the checkout to the v1.1.0 tag; see NOTICE). Run `cargo fmt`
afterwards; the committed file is formatted."""
import pathlib
import re
import sys

if len(sys.argv) != 2:
    sys.exit(__doc__)
SRC = sys.argv[1]
OUT = pathlib.Path(__file__).resolve().parent.parent / "crates/iamf-dec/src/matrices.rs"

TOKEN_MAP = {
    "IAMF_MONO": "Mono", "IAMF_STEREO": "Stereo", "IAMF_312": "Iamf312",
    "IAMF_51": "Iamf51", "IAMF_512": "Iamf512", "IAMF_514": "Iamf514",
    "IAMF_71": "Iamf71", "IAMF_712": "Iamf712", "IAMF_714": "Iamf714",
    "IAMF_916": "Iamf916", "IAMF_BINAURAL": "Binaural",
    "BS2051_A": "Bs2051A", "BS2051_B": "Bs2051B", "BS2051_C": "Bs2051C",
    "BS2051_D": "Bs2051D", "BS2051_E": "Bs2051E", "BS2051_F": "Bs2051F",
    "BS2051_G": "Bs2051G", "BS2051_H": "Bs2051H", "BS2051_I": "Bs2051I",
    "BS2051_J": "Bs2051J",
    "IAMF_ZOA": "Zoa", "IAMF_FOA": "Foa", "IAMF_SOA": "Soa", "IAMF_TOA": "Toa",
    "IAMF_H4A": "H4a", "BS2051_M": "Bs2051M",
}

def strip_comments(text):
    text = re.sub(r"//[^\n]*", "", text)
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    return text

NUM = r"-?(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?"

def parse_matrices(text):
    """name -> (declared_cols|None, rows) where rows is a list of value
    lists (one per inner brace group), or a single flat list for 1-D."""
    mats = {}
    for m in re.finditer(r"float\s+(\w+)\s*\[\]\s*(?:\[(\d+)\])?\s*=\s*\{(.*?)\};",
                         text, flags=re.S):
        name, cols, body = m.group(1), m.group(2), m.group(3)
        if cols:
            rows = [[float(x) for x in re.findall(NUM, row)]
                    for row in re.findall(r"\{([^{}]*)\}", body)]
            mats[name] = (int(cols), rows)
        else:
            mats[name] = (None, [[float(x) for x in re.findall(NUM, body)]])
    return mats

def assemble_m2m(name, mats, m, n):
    """C semantics: rows padded to the declared column count (stereo_bs070
    has a short row that C zero-fills)."""
    cols, rows = mats[name]
    assert cols == n and len(rows) == m, (name, cols, len(rows), m, n)
    vals = []
    for row in rows:
        assert len(row) <= cols, (name, len(row), cols)
        vals.extend(row + [0.0] * (cols - len(row)))
    return vals

def assemble_h2m(name, mats, m, n):
    """Tightly packed [n out rows][m in cols], as the render code indexes it.
    soa_bs9A3 is declared [][22] in libiamf but its rows carry 9 (= m)
    values; the C code indexes tightly, so its padded memory diverges from
    the intended matrix for that entry (an upstream bug). We extract the
    intended tight matrix from the row values."""
    cols, rows = mats[name]
    if cols is None:
        vals = rows[0]
        assert len(vals) == m * n, (name, len(vals), m, n)
        return vals
    assert len(rows) == n, (name, len(rows), n)
    vals = []
    for row in rows:
        assert len(row) <= m, (name, len(row), m)
        vals.extend(row + [0.0] * (m - len(row)))
    return vals

def rust_float(x):
    s = repr(x)
    if "." not in s and "e" not in s and "E" not in s:
        s += ".0"
    return s

def main():
    out = []
    out.append("""//! Rendering gain matrices, generated from libiamf v1.1.0
//! (code/src/iamf_dec/{m2m_rdr.c,h2m_rdr.c}) by tools/extract_matrices.py.
//! Do not edit by hand; regenerate against the pinned reference instead.
//!
//! M2M matrices are indexed `mat[input_ch * n + output_ch]`; H2M matrices
//! are indexed `mat[output_ch * m + input_acn]` and omit LFE channels,
//! whose positions in the full output layout are `lfe1`/`lfe2`.
#![allow(clippy::excessive_precision, clippy::approx_constant)]

/// Identifies a channel layout as used by the rendering matrix tables:
/// IAMF loudspeaker layouts as inputs, BS.2051 sound systems (plus IAMF
/// extensions) as outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixLayout {
    Mono, Stereo, Iamf312, Iamf51, Iamf512, Iamf514, Iamf71, Iamf712,
    Iamf714, Iamf916, Binaural,
    Bs2051A, Bs2051B, Bs2051C, Bs2051D, Bs2051E, Bs2051F, Bs2051G,
    Bs2051H, Bs2051I, Bs2051J, Bs2051M,
}

/// Ambisonics order of a scene-based input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoaOrder { Zoa, Foa, Soa, Toa, H4a }

pub struct M2m {
    pub input: MatrixLayout,
    pub output: MatrixLayout,
    pub mat: &'static [f32],
    /// Input channel count (rows).
    pub m: usize,
    /// Output channel count (columns).
    pub n: usize,
}

pub struct H2m {
    pub input: HoaOrder,
    pub output: MatrixLayout,
    /// Channel count of the full output layout including LFEs.
    pub total_channels: usize,
    /// LFE positions in the full output layout (rendered silent), or None.
    pub lfe1: Option<usize>,
    pub lfe2: Option<usize>,
    pub mat: &'static [f32],
    /// Input (ACN) channel count.
    pub m: usize,
    /// Output channel count excluding LFEs.
    pub n: usize,
}
""")

    # ---- M2M ----
    text = strip_comments(open(f"{SRC}/m2m_rdr.c").read())
    mats = parse_matrices(text)
    tab_body = re.search(r"struct m2m_rdr_t m2m_rdr_tab\[\]\s*=\s*\{(.*?)\}\s*;",
                         text, flags=re.S).group(1)
    entries = re.findall(
        r"\{\s*(\w+)\s*,\s*(\w+)\s*,\s*\(float \*\)(\w+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\}",
        tab_body)
    used = []
    seen = set()
    for (i, o, name, m, n) in entries:
        if name not in seen:
            seen.add(name)
            vals = assemble_m2m(name, mats, int(m), int(n))
            used.append((name, vals))
    for name, vals in used:
        out.append(f"static {name.upper()}: [f32; {len(vals)}] = [{', '.join(rust_float(v) for v in vals)}];")
    out.append("")
    out.append(f"pub static M2M_TABLE: [M2m; {len(entries)}] = [")
    for (i, o, name, m, n) in entries:
        out.append(f"    M2m {{ input: MatrixLayout::{TOKEN_MAP[i]}, output: MatrixLayout::{TOKEN_MAP[o]}, mat: &{name.upper()}, m: {m}, n: {n} }},")
    out.append("];")
    out.append("")
    m2m_count = len(entries)

    # ---- H2M ----
    text = strip_comments(open(f"{SRC}/h2m_rdr.c").read())
    mats = parse_matrices(text)
    tab_body = re.search(r"struct h2m_rdr_t h2m_rdr_tab\[\]\s*=\s*\{(.*?)\}\s*;",
                         text, flags=re.S).group(1)
    entries = re.findall(
        r"\{\s*(\w+)\s*,\s*(\w+)\s*,\s*(\d+)\s*,\s*(-?\d+)\s*,\s*(-?\d+)\s*,\s*\(float \*\)(\w+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\}",
        tab_body)
    seen = set()
    used = []
    for (i, o, ch, l1, l2, name, m, n) in entries:
        if name not in seen:
            seen.add(name)
            vals = assemble_h2m(name, mats, int(m), int(n))
            used.append((name, vals))
    for name, vals in used:
        out.append(f"static {name.upper()}: [f32; {len(vals)}] = [{', '.join(rust_float(v) for v in vals)}];")
    out.append("")
    out.append(f"pub static H2M_TABLE: [H2m; {len(entries)}] = [")
    for (i, o, ch, l1, l2, name, m, n) in entries:
        lfe1 = "None" if l1 == "-1" else f"Some({l1})"
        lfe2 = "None" if l2 == "-1" else f"Some({l2})"
        out.append(f"    H2m {{ input: HoaOrder::{TOKEN_MAP[i]}, output: MatrixLayout::{TOKEN_MAP[o]}, total_channels: {ch}, lfe1: {lfe1}, lfe2: {lfe2}, mat: &{name.upper()}, m: {m}, n: {n} }},")
    out.append("];")
    out.append("")

    with open(OUT, "w") as f:
        f.write("\n".join(out))
    print(f"wrote {OUT}: {m2m_count} m2m entries, {len(entries)} h2m entries")

if __name__ == "__main__":
    main()
