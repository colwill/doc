//! The radar as inline SVG with the platform's `doc-radar` classes. Each quadrant-and-ring segment
//! packs its blips along arcs, spread evenly and kept clear of the axes, so the same entries are
//! always drawn in the same places and none overlap until a segment is very full.

use std::f64::consts::FRAC_PI_2;
use std::fmt::Write;

use crate::model::{Moved, QUADRANTS, RINGS};

/// The outer ring's radius, and the room around it for the quadrants' names. The radar is drawn
/// at twice the size it is shown at (`doc-radar` halves it), so everything on it — blips, the
/// room between them, the labels — is set here at twice what it should read as. It is a picture
/// to take in at a glance, and at full width it was mostly empty circle.
const RADIUS: f64 = 360.0;
const SIDE: f64 = 28.0;
const TOP: f64 = 60.0;
/// Each ring's outer edge, as a share of the radius, from Adopt out.
const RING_EDGES: [f64; 4] = [0.34, 0.56, 0.78, 1.0];
/// A blip's radius; the distance between blips' centres while a segment has room for it; and the
/// least that shrinks to, which still keeps blips apart. Past that, a segment's blips overlap.
const BLIP: f64 = 12.0;
const SPACING: f64 = 32.0;
const LEAST_SPACING: f64 = 2.0 * BLIP + 2.0;
/// How far above the horizontal axis the ring labels reach.
const LABEL_CLEAR: f64 = 16.0;

pub struct Blip {
    pub number: usize,
    pub title: String,
    pub quadrant: usize,
    pub ring: usize,
    pub moved: Moved,
    pub href: String,
    /// Drawn faded, so what the page is about stands out.
    pub faded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

pub fn width() -> f64 {
    2.0 * (RADIUS + SIDE)
}

pub fn height() -> f64 {
    2.0 * (RADIUS + TOP)
}

fn centre() -> Point {
    Point { x: RADIUS + SIDE, y: RADIUS + TOP }
}

/// A quadrant's angles, anticlockwise from the right, in radians: top left, top right, bottom left,
/// bottom right, as `QUADRANTS` lists them.
fn angles(quadrant: usize) -> (f64, f64) {
    let start = match quadrant {
        0 => 1.0,
        1 => 0.0,
        2 => 2.0,
        _ => 3.0,
    } * FRAC_PI_2;
    (start, start + FRAC_PI_2)
}

fn band(ring: usize) -> (f64, f64) {
    let inner = if ring == 0 { 0.0 } else { RING_EDGES[ring - 1] * RADIUS };
    (inner, RING_EDGES[ring] * RADIUS)
}

/// An arc a segment's blips sit on: its radius, the angles its blips keep clear of each end, in
/// radians, and how many blips it holds.
struct Arc {
    radius: f64,
    before: f64,
    after: f64,
    holds: usize,
}

/// The arcs a segment's blips sit on at a spacing. `clear` is the extra room, in pixels, kept at
/// each end of the segment's angle, for the ring labels along the horizontal axis.
fn arcs(inner: f64, outer: f64, span: f64, spacing: f64, clear: (f64, f64)) -> Vec<Arc> {
    let mut arcs = Vec::new();
    // The centre is crowded by every quadrant's axis, so the first arc starts further out.
    let mut radius = if inner == 0.0 { spacing * 1.6 } else { inner + spacing * 0.7 };
    while radius <= outer - spacing * 0.5 {
        let (before, after) =
            ((spacing * 0.6 + clear.0) / radius, (spacing * 0.6 + clear.1) / radius);
        let usable = span - before - after;
        // Spread over the arc, `n` blips are `usable / n` apart, so it holds as many as keep that
        // at least the spacing; and one, in the middle, whenever there is room at all.
        if usable >= 0.0 {
            let holds = ((usable * radius / spacing).floor() as usize).max(1);
            arcs.push(Arc { radius, before, after, holds });
        }
        radius += spacing;
    }
    arcs
}

/// Shares `count` among arcs in proportion to what each holds, largest remainders first.
fn share(count: usize, holds: &[usize]) -> Vec<usize> {
    let room: usize = holds.iter().sum();
    if room == 0 {
        return holds.iter().map(|_| 0).collect();
    }
    let exact: Vec<f64> = holds.iter().map(|hold| (count * hold) as f64 / room as f64).collect();
    let mut shares: Vec<usize> =
        exact.iter().zip(holds).map(|(exact, hold)| (exact.floor() as usize).min(*hold)).collect();
    let mut order: Vec<usize> = (0..holds.len()).collect();
    order.sort_by(|a, b| {
        (exact[*b] - exact[*b].floor()).total_cmp(&(exact[*a] - exact[*a].floor()))
    });
    let mut left = count.saturating_sub(shares.iter().sum());
    while left > 0 {
        let before = left;
        for &arc in &order {
            if left > 0 && shares[arc] < holds[arc] {
                shares[arc] += 1;
                left -= 1;
            }
        }
        if left == before {
            break;
        }
    }
    // More than fits at the least spacing: the outermost arc takes the rest, overlapping.
    if let Some(last) = shares.last_mut() {
        *last += left;
    }
    shares
}

/// Where `count` blips go in a segment, in order from the inside out.
pub fn place(quadrant: usize, ring: usize, count: usize) -> Vec<Point> {
    if count == 0 {
        return Vec::new();
    }
    let (start, end) = angles(quadrant);
    let (inner, outer) = band(ring);
    // The ring labels sit just above the horizontal axis: at the start of the top right quadrant's
    // angle, and at the end of the top left's.
    let clear = match quadrant {
        0 => (0.0, LABEL_CLEAR),
        1 => (LABEL_CLEAR, 0.0),
        _ => (0.0, 0.0),
    };
    let mut spacing = SPACING;
    let mut laid = arcs(inner, outer, end - start, spacing, clear);
    while laid.iter().map(|arc| arc.holds).sum::<usize>() < count && spacing > LEAST_SPACING {
        spacing = (spacing * 0.92).max(LEAST_SPACING);
        laid = arcs(inner, outer, end - start, spacing, clear);
    }
    if laid.is_empty() {
        laid.push(Arc { radius: (inner + outer) / 2.0, before: 0.0, after: 0.0, holds: count });
    }
    let holds: Vec<usize> = laid.iter().map(|arc| arc.holds).collect();
    let centre = centre();
    let mut points = Vec::with_capacity(count);
    for (arc, on) in laid.iter().zip(share(count, &holds)) {
        let usable = (end - start) - arc.before - arc.after;
        for index in 0..on {
            let angle = start + arc.before + usable * (index as f64 + 0.5) / on as f64;
            points.push(Point {
                x: centre.x + arc.radius * angle.cos(),
                y: centre.y - arc.radius * angle.sin(),
            });
        }
    }
    points
}

pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn round(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn shape(moved: Moved, at: Point) -> String {
    let (x, y) = (round(at.x), round(at.y));
    let size = BLIP + 2.0;
    match moved {
        Moved::In => format!(
            r#"<path class="doc-radar__blip" d="M {x} {} L {} {} L {} {} Z" />"#,
            round(y - size),
            round(x + size),
            round(y + size * 0.7),
            round(x - size),
            round(y + size * 0.7)
        ),
        Moved::Out => format!(
            r#"<path class="doc-radar__blip" d="M {x} {} L {} {} L {} {} Z" />"#,
            round(y + size),
            round(x + size),
            round(y - size * 0.7),
            round(x - size),
            round(y - size * 0.7)
        ),
        Moved::New => format!(
            r#"<circle class="doc-radar__halo" cx="{x}" cy="{y}" r="{}" /><circle class="doc-radar__blip" cx="{x}" cy="{y}" r="{BLIP}" />"#,
            BLIP + 2.5
        ),
        Moved::None => {
            format!(r#"<circle class="doc-radar__blip" cx="{x}" cy="{y}" r="{BLIP}" />"#)
        }
    }
}

/// The whole radar. `blips` may come in any order; each is drawn in its own segment.
pub fn draw(blips: &[Blip], description: &str) -> String {
    let (width, height, centre) = (width(), height(), centre());
    let mut svg = String::new();
    let _ = write!(
        svg,
        r#"<svg class="doc-radar" viewBox="0 0 {width} {height}" role="img" aria-label="{}">"#,
        escape(description)
    );
    for ring in (0..RINGS.len()).rev() {
        let class = match ring % 2 {
            0 => "doc-radar__ring doc-radar__ring--shaded",
            _ => "doc-radar__ring",
        };
        let _ = write!(
            svg,
            r#"<circle class="{class}" cx="{}" cy="{}" r="{}" />"#,
            centre.x,
            centre.y,
            band(ring).1
        );
    }
    let _ = write!(
        svg,
        r#"<path class="doc-radar__axis" d="M {} {} H {} M {} {} V {}" />"#,
        centre.x - RADIUS,
        centre.y,
        centre.x + RADIUS,
        centre.x,
        centre.y - RADIUS,
        centre.y + RADIUS
    );
    for (ring, shown) in RINGS.iter().enumerate() {
        let (inner, outer) = band(ring);
        let middle = round((inner + outer) / 2.0);
        for x in [centre.x - middle, centre.x + middle] {
            let _ = write!(
                svg,
                r#"<text class="doc-radar__ring-label" x="{x}" y="{}" text-anchor="middle">{}</text>"#,
                centre.y - 6.0,
                escape(shown.name)
            );
        }
    }
    let corners = [
        (SIDE, 26.0, "start"),
        (width - SIDE, 26.0, "end"),
        (SIDE, height - 14.0, "start"),
        (width - SIDE, height - 14.0, "end"),
    ];
    for (index, (quadrant, (x, y, anchor))) in QUADRANTS.iter().zip(corners).enumerate() {
        let _ = write!(
            svg,
            r#"<text class="doc-radar__quadrant doc-radar__colour--{}" x="{x}" y="{y}" text-anchor="{anchor}">{}</text>"#,
            index + 1,
            escape(quadrant.name)
        );
    }
    for quadrant in 0..QUADRANTS.len() {
        for (ring, shown) in RINGS.iter().enumerate() {
            let mut here: Vec<&Blip> = blips
                .iter()
                .filter(|blip| blip.quadrant == quadrant && blip.ring == ring)
                .collect();
            here.sort_by_key(|blip| blip.number);
            for (blip, at) in here.iter().zip(place(quadrant, ring, here.len())) {
                let class = match blip.faded {
                    true => format!(
                        "doc-radar__entry doc-radar__entry--faded doc-radar__colour--{}",
                        quadrant + 1
                    ),
                    false => format!("doc-radar__entry doc-radar__colour--{}", quadrant + 1),
                };
                // A triangle's middle is not its centre: the number sits in its wider part.
                let nudge = match blip.moved {
                    Moved::In => 3.0,
                    Moved::Out => -3.0,
                    Moved::New | Moved::None => 0.0,
                };
                let moved = match blip.moved.words() {
                    "" => String::new(),
                    words => format!(", {words}"),
                };
                let _ = write!(
                    svg,
                    r#"<a class="{class}" href="{}"><title>{}. {} ({}{moved})</title>{}<text class="doc-radar__number" x="{}" y="{}" text-anchor="middle" dominant-baseline="central">{}</text></a>"#,
                    escape(&blip.href),
                    blip.number,
                    escape(&blip.title),
                    shown.name,
                    shape(blip.moved, at),
                    round(at.x),
                    round(at.y + nudge),
                    blip.number
                );
            }
        }
    }
    svg.push_str("</svg>");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn distance(a: Point, b: Point) -> f64 {
        ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
    }

    /// Every blip is inside its own segment, clear of its edges by at least half a blip.
    fn inside(quadrant: usize, ring: usize, point: Point) -> bool {
        let centre = centre();
        let (dx, dy) = (point.x - centre.x, centre.y - point.y);
        let radius = (dx * dx + dy * dy).sqrt();
        let mut angle = dy.atan2(dx);
        if angle < 0.0 {
            angle += 4.0 * FRAC_PI_2;
        }
        let (start, end) = angles(quadrant);
        let (inner, outer) = band(ring);
        let clear = BLIP / 2.0;
        let from_axes = (radius * (angle - start).sin()).min(radius * (end - angle).sin());
        radius > inner + clear && radius < outer - clear && from_axes > clear
    }

    /// The most a segment holds apart, at the least spacing.
    fn room(quadrant: usize, ring: usize) -> usize {
        let (start, end) = angles(quadrant);
        let (inner, outer) = band(ring);
        let clear = match quadrant {
            0 => (0.0, LABEL_CLEAR),
            1 => (LABEL_CLEAR, 0.0),
            _ => (0.0, 0.0),
        };
        arcs(inner, outer, end - start, LEAST_SPACING, clear).iter().map(|arc| arc.holds).sum()
    }

    #[test]
    fn segments_hold_their_blips_apart() {
        for quadrant in 0..4 {
            for ring in 0..4 {
                // Adopt, the smallest, still holds a good few.
                // Adopt, the smallest segment, holds six at the least spacing — twenty-four
                // across the ring and ninety-six on the radar, more than any real one carries.
                // It held more when the radar was drawn at the size it was shown at; halving
                // that while keeping a blip readable is what it costs, and it is worth it.
                assert!(
                    room(quadrant, ring) >= 6,
                    "{quadrant}/{ring} holds {}",
                    room(quadrant, ring)
                );
                for count in [1, 2, 5, room(quadrant, ring)] {
                    let points = place(quadrant, ring, count);
                    assert_eq!(points.len(), count);
                    for (index, point) in points.iter().enumerate() {
                        assert!(
                            inside(quadrant, ring, *point),
                            "{quadrant}/{ring}/{count}: {point:?}"
                        );
                        for other in &points[index + 1..] {
                            assert!(
                                distance(*point, *other) >= 2.0 * BLIP,
                                "{quadrant}/{ring}/{count}: {point:?} and {other:?} overlap"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_crowded_segment_still_places_everything() {
        assert_eq!(place(3, 3, 200).len(), 200);
    }

    #[test]
    fn shares_are_whole_and_within_room() {
        assert_eq!(share(5, &[3, 4, 5]).iter().sum::<usize>(), 5);
        assert_eq!(share(1, &[3, 4, 5]).iter().sum::<usize>(), 1);
        assert_eq!(share(12, &[3, 4, 5]), vec![3, 4, 5]);
        assert_eq!(share(14, &[3, 4, 5]), vec![3, 4, 7]);
    }
}
