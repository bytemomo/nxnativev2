//! Renders the UI mockup (ui/mockup/mockup.slint) to PNG files, one per page:
//! `cargo run -p nxapp --example mockup -- <out dir> <icon of game 1> <icon of game 2> ...`

use slint::{ComponentHandle, Image, ModelRc, SharedString, VecModel};

slint::slint! {
    export { Mockup, GameRow } from "ui/mockup/mockup.slint";
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = std::path::PathBuf::from(args.first().cloned().unwrap_or_else(|| ".".into()));
    let icon = |i: usize| args.get(1 + i).and_then(|p| Image::load_from_path(std::path::Path::new(p)).ok()).unwrap_or_default();
    let rows = |building: bool| {
        vec![
            GameRow { name: "Hollow Knight: Silksong".into(), icon: icon(0), state: 3, detail: "Standalone · in LiveContainer".into(), fraction: 0.0 },
            GameRow {
                name: "Mario Kart 8 Deluxe".into(),
                icon: icon(1),
                state: if building { 1 } else { 0 },
                detail: if building { "Building · 68%".into() } else { "Ready to install".into() },
                fraction: 0.68,
            },
            GameRow { name: "Celeste".into(), icon: icon(2), state: 2, detail: "Needs prod.keys".into(), fraction: 0.0 },
        ]
    };
    std::fs::create_dir_all(&out)?;
    for (page, sel, title, facts, file) in [
        (0, 0usize, "Hollow Knight: Silksong", "Version 1.0.30000  ·  6.4 GB  ·  2,449 shaders", "1-game-ready.png"),
        (1, 1usize, "Mario Kart 8 Deluxe", "Version 13.0.1  ·  6.9 GB  ·  29,824 shaders", "2-game-building.png"),
        (2, 0usize, "", "", "3-livecontainer.png"),
        (3, 0usize, "", "", "4-ipad-and-apple-id.png"),
        (4, 0usize, "", "", "5-settings.png"),
    ] {
        let ui = Mockup::new()?;
        ui.set_games(ModelRc::new(VecModel::from(rows(page == 1))));
        ui.set_selected(sel as i32);
        ui.set_hero(icon(sel));
        ui.set_page(page);
        ui.set_game_title(SharedString::from(title));
        ui.set_facts(SharedString::from(facts));
        ui.set_lc_icons(ModelRc::new(VecModel::from(vec![icon(0), icon(1)])));
        ui.show()?;
        let weak = ui.as_weak();
        let path = out.join(file);
        slint::Timer::single_shot(std::time::Duration::from_millis(400), move || {
            if let Some(ui) = weak.upgrade() {
                match ui.window().take_snapshot() {
                    Ok(px) => {
                        let f = std::fs::File::create(&path).unwrap();
                        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), px.width(), px.height());
                        enc.set_color(png::ColorType::Rgba);
                        enc.set_depth(png::BitDepth::Eight);
                        enc.write_header().unwrap().write_image_data(px.as_bytes()).unwrap();
                        println!("{}", path.display());
                    }
                    Err(e) => eprintln!("snapshot: {e}"),
                }
                let _ = ui.hide();
            }
            let _ = slint::quit_event_loop();
        });
        slint::run_event_loop()?;
    }
    Ok(())
}
