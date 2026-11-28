// Paint the approved Living Paper SVG's sampled paths directly on the GPUI canvas.
use gpui::{
    Bounds, Context, PathBuilder, Pixels, Render, Window, canvas, div, point, prelude::*, px, rgb,
};
use roxmltree::{Document, Node};
use std::{rc::Rc, time::Instant};
use svgtypes::{PathParser, PathSegment};

struct Layer {
    segments: Vec<PathSegment>,
    color: u32,
    stroke: bool,
}
pub struct Artwork {
    frames: Vec<Vec<Layer>>,
    seconds: f32,
}
fn samples<'a>(node: Node<'a, '_>, key: &str) -> Option<Vec<&'a str>> {
    node.children()
        .find(|n| n.attribute("attributeName") == Some(key))?
        .attribute("values")
        .map(|s| s.split(';').collect())
}
impl Artwork {
    pub fn load() -> Rc<Self> {
        let doc = Document::parse(include_str!("../assets/living-paper.svg")).expect("bundled SVG");
        let mut frames: Vec<Vec<Layer>> = Vec::new();
        let duration = doc.descendants().find_map(|n| n.attribute("dur")).unwrap();
        for path in doc.descendants().filter(|n| n.has_tag_name("path")) {
            let shapes = samples(path, "d").expect("animated path");
            if frames.is_empty() {
                frames.resize_with(shapes.len(), Vec::new);
            }
            assert_eq!(shapes.len(), frames.len());
            let colors = samples(path, "fill");
            for (i, d) in shapes.iter().enumerate() {
                let stroke = path.attribute("stroke");
                let color = stroke
                    .or_else(|| colors.as_ref().map(|list| list[i]))
                    .or(path.attribute("fill"))
                    .unwrap();
                frames[i].push(Layer {
                    segments: PathParser::from(*d)
                        .collect::<Result<_, _>>()
                        .expect("valid vector data"),
                    color: u32::from_str_radix(color.trim_start_matches('#'), 16).unwrap(),
                    stroke: stroke.is_some(),
                });
            }
        }
        Rc::new(Self {
            frames,
            seconds: duration.trim_end_matches('s').parse().unwrap(),
        })
    }
    fn index(&self, time: f32) -> usize {
        ((time / self.seconds).clamp(0., 1.) * (self.frames.len() - 1) as f32).round() as usize
    }
    pub fn picture(
        self: &Rc<Self>,
        time: f32,
        width: f32,
        height: f32,
    ) -> impl IntoElement + use<> {
        let art = self.clone();
        let index = self.index(time);
        canvas(
            |_, _, _| (),
            move |bounds: Bounds<Pixels>, _, win, _| {
                let scale =
                    (f32::from(bounds.size.width) / 292.).min(f32::from(bounds.size.height) / 252.);
                let origin = bounds.center() - point(px(146. * scale), px(126. * scale));
                let xy =
                    |x: f64, y: f64| origin + point(px(x as f32 * scale), px(y as f32 * scale));
                for layer in &art.frames[index] {
                    let mut builder = if layer.stroke {
                        PathBuilder::stroke(px(2. * scale))
                    } else {
                        PathBuilder::fill()
                    };
                    for segment in &layer.segments {
                        match *segment {
                            PathSegment::MoveTo { abs: true, x, y } => builder.move_to(xy(x, y)),
                            PathSegment::Quadratic {
                                abs: true,
                                x1,
                                y1,
                                x,
                                y,
                            } => builder.curve_to(xy(x, y), xy(x1, y1)),
                            PathSegment::CurveTo {
                                abs: true,
                                x1,
                                y1,
                                x2,
                                y2,
                                x,
                                y,
                            } => builder.cubic_bezier_to(xy(x, y), xy(x1, y1), xy(x2, y2)),
                            PathSegment::ClosePath { .. } => builder.close(),
                            _ => unreachable!(
                                "unsupported path segment in embedded Living Paper artwork"
                            ),
                        }
                    }
                    if let Ok(path) = builder.build() {
                        win.paint_path(path, rgb(layer.color));
                    }
                }
            },
        )
        .w(px(width))
        .h(px(height))
        .flex_none()
    }
}

pub struct Character {
    art: Rc<Artwork>,
    size: (f32, f32),
    start: Instant,
    from: f32,
    until: f32,
}
impl Character {
    pub fn still(&self, width: f32, height: f32) -> impl IntoElement + use<> {
        self.art.picture(2.05, width, height)
    }

    pub fn new(art: Rc<Artwork>, size: (f32, f32)) -> Self {
        Self {
            art,
            size,
            start: Instant::now(),
            from: 0.,
            until: 2.05,
        }
    }
    pub fn play(&mut self, from: f32, until: f32, cx: &mut Context<Self>) {
        self.start = Instant::now();
        self.from = from;
        self.until = until;
        cx.notify();
    }
}
impl Render for Character {
    fn render(&mut self, win: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = self.from + self.start.elapsed().as_secs_f32();
        if t < self.until {
            win.request_animation_frame();
        }
        div()
            .id("living-paper")
            .cursor_pointer()
            .child(
                self.art
                    .picture(t.min(self.until), self.size.0, self.size.1),
            )
            .on_click(cx.listener(|this, _, _, cx| this.play(0., 8.8, cx)))
    }
}
