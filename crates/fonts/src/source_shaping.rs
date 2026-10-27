//! Shaping an explicitly selected source/document face, with OpenType features and UAX #9.
use harfrust::{BufferClusterLevel, Direction, ShapeOptions, ShaperData, UnicodeBuffer};
use skrifa::raw::TableProvider;
use skrifa::{FontRef, GlyphId, MetadataProvider};
use unicode_bidi::{Level, ParagraphBidiInfo};

#[derive(Clone, Debug)]
pub struct ShapedGlyph {
    pub id: GlyphId,
    pub position: [f64; 2],
    /// The characters this glyph stands for in `/ToUnicode`: its own in a left-to-right cluster
    /// whose glyphs each take one, else the whole cluster on its first glyph and none on the rest.
    pub text: String,
    /// On the first glyph of a left-to-right cluster of several glyphs (a letter and its marks,
    /// Sara Am drawn as two glyphs): the cluster's text and glyph count, for an `/ActualText`
    /// span around just that cluster, as Chrome writes them. Readers that honour it read the
    /// marks in typing order wherever shaping put them; the rest read `text`.
    pub span: Option<(String, usize)>,
}
#[derive(Clone, Debug)]
pub struct ShapedRun {
    pub glyphs: Vec<ShapedGlyph>,
    pub advance: f64,
    /// Whether any of it is right-to-left: drawn in visual order, it then needs one `/ActualText`
    /// in typing order around the whole run.
    pub rtl: bool,
}

/// Complex scripts/marks require shaping instead of independently encoding characters.
pub fn required(text: &str) -> bool {
    text.chars()
        .any(|c| matches!(u32::from(c), 0x0300..=0x036f | 0x0590..=0x0fff | 0x1780..=0x17ff | 0x200c..=0x200f | 0x202a..=0x202e | 0x2066..=0x2069))
}

pub fn shape(bytes: &[u8], text: &str) -> Result<ShapedRun, String> {
    if text.len() > 262_144 || text.contains(['\n', '\r']) {
        return Err("shape one bounded text row at a time".into());
    }
    if text.is_empty() {
        return Ok(ShapedRun { glyphs: Vec::new(), advance: 0.0, rtl: false });
    }
    let face = FontRef::new(bytes).map_err(|e| e.to_string())?;
    if text.chars().any(|c| matches!(u32::from(c), 0x0600..=0x06ff | 0x0750..=0x077f | 0x08a0..=0x0fff | 0x1780..=0x17ff)) && face.gsub().is_err() {
        return Err("the source subset has no contextual substitution tables; select its full font".into());
    }
    let data = ShaperData::new(&face);
    let shaper = data.shaper(&face).build();
    let em = f64::from(shaper.units_per_em().max(1));
    let base = if unicode_bidi::get_base_direction(text) == unicode_bidi::Direction::Rtl { Level::rtl() } else { Level::ltr() };
    let bidi = ParagraphBidiInfo::new(text, Some(base));
    let (levels, runs) = bidi.visual_runs(0..text.len());
    let charmap = face.charmap();
    let mut glyphs: Vec<ShapedGlyph> = Vec::new();
    let mut x = 0.0;
    let mut any_rtl = false;
    for run in runs {
        let Some(input) = text.get(run.clone()) else { continue };
        let rtl = levels.get(run.start).is_some_and(Level::is_rtl);
        any_rtl |= rtl;
        let run_start = glyphs.len();
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(input);
        buffer.set_cluster_level(BufferClusterLevel::MonotoneGraphemes);
        buffer.set_direction(if rtl { Direction::RightToLeft } else { Direction::LeftToRight });
        buffer.guess_segment_properties();
        let output = shaper.shape(buffer, ShapeOptions::new());
        let mut starts: Vec<_> = output.glyph_infos().iter().map(|g| g.cluster as usize).collect();
        starts.sort_unstable();
        starts.dedup();
        let mut seen = std::collections::HashSet::new();
        for (g, p) in output.glyph_infos().iter().zip(output.glyph_positions()) {
            if g.glyph_id == 0 {
                return Err("the full font lacks a required contextual glyph; choose the complete source face".into());
            }
            let start = g.cluster as usize;
            let end = starts.get(starts.partition_point(|i| *i <= start)).copied().unwrap_or(input.len());
            let logical = if seen.insert(start) { input.get(start..end).unwrap_or_default().to_string() } else { String::new() };
            glyphs.push(ShapedGlyph {
                id: GlyphId::new(g.glyph_id),
                position: [x + f64::from(p.x_offset) / em, f64::from(p.y_offset) / em],
                text: logical,
                span: None,
            });
            x += f64::from(p.x_advance) / em;
        }
        if !rtl {
            let clusters: Vec<u32> = output.glyph_infos().iter().map(|g| g.cluster).collect();
            split_clusters(glyphs.get_mut(run_start..).unwrap_or_default(), &clusters, |c| charmap.map(c));
        }
    }
    if !x.is_finite() || glyphs.len() > 65_534 {
        return Err("shaped row exceeds its geometry budget".into());
    }
    Ok(ShapedRun { glyphs, advance: x, rtl: any_rtl })
}

/// Mark each left-to-right cluster of several glyphs for a span, and give each of its glyphs its
/// own character when the cluster has as many glyphs as characters and they correspond in order:
/// every glyph is its character's own (the face's cmap), or the cluster is Thai, whose shaping
/// substitutes mark forms one for one and never reorders them (Sara Am, split into two glyphs,
/// has one more glyph than characters). Otherwise the text stays on the first glyph. `glyphs` and
/// `clusters` (harfrust's cluster of each) are one run, in order; each cluster's text is already
/// on its first glyph.
fn split_clusters(glyphs: &mut [ShapedGlyph], clusters: &[u32], nominal: impl Fn(char) -> Option<GlyphId>) {
    let mut at = 0;
    while at < glyphs.len() {
        let cluster = clusters.get(at).copied();
        let len = clusters.get(at..).unwrap_or_default().iter().take_while(|c| Some(**c) == cluster).count().max(1);
        let Some(group) = glyphs.get_mut(at..at + len) else { break };
        at += len;
        if group.len() < 2 {
            continue;
        }
        let text = group.first().map(|g| g.text.clone()).unwrap_or_default();
        let chars: Vec<char> = text.chars().collect();
        let thai = chars.iter().all(|c| matches!(u32::from(*c), 0x0e01..=0x0e5b));
        if chars.len() == group.len() && (thai || group.iter().zip(&chars).all(|(g, c)| nominal(*c) == Some(g.id))) {
            for (glyph, c) in group.iter_mut().zip(chars) {
                glyph.text = c.to_string();
            }
        }
        if let Some(first) = group.first_mut() {
            first.span = Some((text, len));
        }
    }
}
