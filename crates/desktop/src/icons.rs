// Small line icons are drawn in the same logical coordinate system at every display scale.
use gpui::{Bounds, PathBuilder, Pixels, canvas, point, prelude::*, px, rgb};
use svgtypes::{PathParser, PathSegment};

#[derive(Clone, Copy)]
pub enum Icon {
    Search,
    New,
    Library,
    History,
    Folder,
    Branch,
    File,
    Down,
    Right,
    Close,
    Arrow,
    Check,
    Plug,
    Fold,
    Gate,
    Copy,
}

impl Icon {
    fn path(self) -> &'static str {
        match self {
            Self::Search => {
                "M16 16 L21 21 M18 10.5 C18 14.65 14.65 18 10.5 18 C6.35 18 3 14.65 3 10.5 C3 6.35 6.35 3 10.5 3 C14.65 3 18 6.35 18 10.5 Z"
            }
            Self::New => {
                "M4 12 L4 19 Q4 21 6 21 L18 21 Q20 21 20 19 L20 13 M4 10 L4 6 Q4 4 6 4 L13 4 M10 15 L11 11 L18.5 3.5 Q19.5 2.5 20.5 3.5 L21 4 Q22 5 21 6 L13.5 13.5 Z"
            }
            Self::Library => {
                "M3 5 L7 5 L7 20 L3 20 Z M10 5 L13.5 5 L13.5 20 L10 20 Z M16 6 L19.2 5 L23 19.2 L19.6 20 Z"
            }
            Self::History => {
                "M3 11 C3.5 6 7 3 12 3 C17 3 21 7 21 12 C21 17 17 21 12 21 C8.5 21 5.5 19 4 16.5 M2 6 L2.8 11.4 L8 10.5 M12 7 L12 12 L15.6 14.5"
            }
            Self::Folder => {
                "M3 7 Q3 5 5 5 L10 5 L12 8 L19 8 Q21 8 21 10 L21 17 Q21 19 19 19 L5 19 Q3 19 3 17 Z"
            }
            Self::Branch => {
                "M6 6 L6 16 Q6 19 9 19 L18 19 M6 11 L13 11 Q16 11 16 8 L16 5 M13 7 L16 4 L19 7"
            }
            Self::File => {
                "M5 3 L14 3 L19 8 L19 21 L5 21 Z M14 3 L14 8 L19 8 M8 12 L16 12 M8 15.5 L14 15.5"
            }
            Self::Down => "M6 9 L12 15 L18 9",
            Self::Right => "M9 6 L15 12 L9 18",
            Self::Close => "M6 6 L18 18 M18 6 L6 18",
            Self::Arrow => "M12 20 L12 4 M5.5 10.5 L12 4 L18.5 10.5",
            Self::Check => "M4 12 L9 17 L20 6",
            Self::Plug => "M8 3 L8 8 M16 3 L16 8 M5 8 L19 8 L19 10 Q19 15 14 16 L12 16 L12 21",
            Self::Fold => "M3 7 L12 3 L21 7 L17 18 L10 21 L3 7 Z M3 7 L12 12 L21 7 M12 12 L10 21",
            Self::Gate => {
                "M12 3 L20 6 L20 12 Q20 17 12 21 Q4 17 4 12 L4 6 Z M8.5 11.5 L11 14 L15.8 9"
            }
            Self::Copy => "M8 7 L20 7 L20 21 L8 21 Z M5 17 L3 17 L3 3 L15 3 L15 5",
        }
    }
    pub fn draw(self, width: f32, color: u32) -> impl IntoElement {
        canvas(
            |_, _, _| (),
            move |b: Bounds<Pixels>, _, w, _| {
                let xy = |x: f64, y: f64| {
                    b.origin + point(px(x as f32 * width / 24.), px(y as f32 * width / 24.))
                };
                let mut p = PathBuilder::stroke(px(width * 0.075));
                for s in PathParser::from(self.path()) {
                    match s.expect("bundled icon path") {
                        PathSegment::MoveTo { x, y, .. } => p.move_to(xy(x, y)),
                        PathSegment::LineTo { x, y, .. } => p.line_to(xy(x, y)),
                        PathSegment::Quadratic { x1, y1, x, y, .. } => {
                            p.curve_to(xy(x, y), xy(x1, y1))
                        }
                        PathSegment::CurveTo {
                            x1,
                            y1,
                            x2,
                            y2,
                            x,
                            y,
                            ..
                        } => p.cubic_bezier_to(xy(x, y), xy(x1, y1), xy(x2, y2)),
                        PathSegment::ClosePath { .. } => p.close(),
                        _ => unreachable!("unsupported icon segment"),
                    }
                }
                if let Ok(path) = p.build() {
                    w.paint_path(path, rgb(color));
                }
            },
        )
        .size(px(width))
        .flex_none()
    }
}
