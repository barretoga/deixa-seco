// Release sem janela de console; debug mantém o console para ver pânicos e logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;

fn main() -> eframe::Result {
    let scan_on_start = deixa_seco::parse_args();
    // Pede admin logo ao abrir (UAC). Se aceitar, este processo encerra e a cópia
    // elevada assume; se recusar, segue sem admin (varredura por pastas).
    if deixa_seco::elevation::should_ask_elevation(
        deixa_seco::elevation::is_elevated(),
        std::env::args().skip(1),
    ) {
        let _ = deixa_seco::elevation::relaunch_elevated(scan_on_start);
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("deixa-seco")
            .with_inner_size([1200.0, 780.0])
            .with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "deixa-seco",
        options,
        Box::new(move |_cc| Ok(Box::new(deixa_seco::ui::App::new(scan_on_start)))),
    )
}
