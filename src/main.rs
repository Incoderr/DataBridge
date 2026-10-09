//! DataBridge entry point: window bootstrap, fonts, theme, root view.

mod core;
mod drivers;
mod ui;

use gpui::prelude::*;
use gpui::{Application, Bounds, Point, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use gpui_component::{Root, ThemeMode};

use ui::app;
use ui::assets;
use ui::theme;

fn main() {
    // CLI: `--mock` runs the built-in demo (seeded connections served by
    // the deterministic mock driver, nothing persisted); without it the
    // app remembers real connections (Stage 3) and connects for real.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "DataBridge — database explorer\n\n\
             Usage: databridge [--mock]\n\n\
             Options:\n  \
             --mock  demo mode: seeded connections on the built-in mock driver,\n\
             \x20         nothing is read from or written to the connection store."
        );
        return;
    }
    let unknown: Vec<&String> = args
        .iter()
        .filter(|a| !matches!(a.as_str(), "--mock"))
        .collect();
    for a in unknown {
        eprintln!("DataBridge: unknown argument {a} (see --help)");
    }
    core::registry::init(args.iter().any(|a| a == "--mock"));

    let fonts: Vec<std::borrow::Cow<'static, [u8]>> = vec![
        include_bytes!("../assets/fonts/PublicSans-Regular.ttf")
            .as_slice()
            .into(),
        include_bytes!("../assets/fonts/PublicSans-Medium.ttf")
            .as_slice()
            .into(),
        include_bytes!("../assets/fonts/PublicSans-SemiBold.ttf")
            .as_slice()
            .into(),
        include_bytes!("../assets/fonts/PublicSans-Bold.ttf")
            .as_slice()
            .into(),
        include_bytes!("../assets/fonts/PublicSans-Italic.ttf")
            .as_slice()
            .into(),
        include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")
            .as_slice()
            .into(),
        include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf")
            .as_slice()
            .into(),
        include_bytes!("../assets/fonts/JetBrainsMono-Italic.ttf")
            .as_slice()
            .into(),
    ];

    Application::new()
        .with_assets(assets::AppAssets)
        .run(move |cx: &mut gpui::App| {
            core::runtime::init();
            cx.text_system().add_fonts(fonts).expect("failed to register fonts");

            gpui_component::init(cx);
            theme::apply(ThemeMode::Dark, None, cx);

            let bounds = Bounds {
                origin: Point {
                    x: px(160.),
                    y: px(100.),
                },
                size: size(px(1280.), px(820.)),
            };
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(760.), px(520.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("DataBridge".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| app::DataBridge::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .expect("failed to open window");
        });
}
