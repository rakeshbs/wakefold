//! Native desktop client for a local or remote Wakefold daemon.
mod avatar;
mod connection;
mod icons;
mod input;
mod local_daemon;
mod markdown;
mod preview;
mod profiles;
mod servers;
mod style;
mod theme;

use gpui::{
    App, Application, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions,
    prelude::*, px, size,
};

actions!(workspace, [SendMessage, Quit]);

fn main() -> anyhow::Result<()> {
    let preview = match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => false,
        [arg] if arg == "--preview" => true,
        [arg] if arg == "--help" || arg == "-h" => {
            println!("Usage: wakefold-desktop [--preview]");
            return Ok(());
        }
        _ => anyhow::bail!("Usage: wakefold-desktop [--preview]"),
    };
    let runtime = tokio::runtime::Runtime::new()?;
    Application::new().run(move |cx: &mut App| {
        theme::initialize(cx);
        input::bind(cx);
        cx.bind_keys([
            KeyBinding::new("enter", SendMessage, Some("TextInput")),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1180.), px(745.)), cx);
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(
                    if preview {
                        "Wakefold Design Preview"
                    } else {
                        "Wakefold"
                    }
                    .into(),
                ),
                ..Default::default()
            }),
            window_min_size: Some(size(px(850.), px(560.))),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        if preview {
            cx.open_window(options, |_, cx| cx.new(preview::Desktop::new))
                .expect("open desktop preview");
        } else {
            cx.open_window(options, move |_, cx| {
                cx.new(|cx| servers::Servers::new(runtime, cx))
            })
            .expect("open desktop");
        }
        cx.activate(true);
    });
    Ok(())
}
