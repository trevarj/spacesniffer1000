fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("spacesniffer1000")
            .with_title("SpaceSniffer1000")
            .with_inner_size([1280.0, 820.0]),
        persist_window: true,
        ..Default::default()
    };

    eframe::run_native(
        "SpaceSniffer1000",
        options,
        Box::new(|cc| Ok(Box::new(spacesniffer1000::App::new(cc)))),
    )
}
