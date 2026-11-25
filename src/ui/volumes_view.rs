//! Tela inicial: um cartão por volume com barra usado/livre e botão "Analisar".

use crate::format::format_bytes;
use crate::volumes::VolumeInfo;
use eframe::egui::{self, Color32, RichText};

/// O que o usuário pediu na tela de volumes.
#[derive(Debug, PartialEq, Eq)]
pub enum VolumesAction {
    /// Analisar o volume com essa letra.
    Scan(char),
    /// Abrir o seletor de pasta.
    PickFolder,
    /// Reabrir como admin e já varrer esse volume pela MFT.
    Elevate(char),
    /// Ler a lista de volumes de novo.
    Refresh,
}

pub fn is_ntfs(v: &VolumeInfo) -> bool {
    v.fs.eq_ignore_ascii_case("NTFS")
}

/// Fração usada (0 a 1); volume de tamanho zero conta como vazio.
pub fn used_fraction(v: &VolumeInfo) -> f32 {
    if v.total == 0 {
        return 0.0;
    }
    (v.total.saturating_sub(v.free) as f64 / v.total as f64).clamp(0.0, 1.0) as f32
}

/// Título do cartão: `C:  Windows · NTFS` (rótulo vazio vira "Disco local").
pub fn volume_title(v: &VolumeInfo) -> String {
    let label = if v.label.trim().is_empty() {
        "Disco local"
    } else {
        v.label.trim()
    };
    format!("{label} · {}", v.fs)
}

pub fn usage_text(v: &VolumeInfo) -> String {
    format!(
        "{} usados de {} · {} livres",
        format_bytes(v.total.saturating_sub(v.free)),
        format_bytes(v.total),
        format_bytes(v.free)
    )
}

pub fn volumes_view(
    ui: &mut egui::Ui,
    volumes: &[VolumeInfo],
    loading: bool,
    elevated: bool,
    message: Option<&str>,
) -> Option<VolumesAction> {
    let mut action = None;
    egui::CentralPanel::default().show(ui, |ui| {
        ui.add_space(12.0);
        ui.heading(RichText::new("deixa-seco").size(26.0).strong());
        ui.label(
            RichText::new("Escolha um disco ou uma pasta para ver o que ocupa espaço.").weak(),
        );
        ui.add_space(8.0);
        if let Some(msg) = message {
            ui.colored_label(ui.visuals().warn_fg_color, msg);
            ui.add_space(4.0);
        }
        ui.horizontal(|ui| {
            if ui.button("📁 Analisar pasta…").clicked() {
                action = Some(VolumesAction::PickFolder);
            }
            let refresh = ui.add_enabled(!loading, egui::Button::new("⟳ Atualizar lista"));
            if refresh.clicked() {
                action = Some(VolumesAction::Refresh);
            }
        });
        ui.add_space(10.0);
        if loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Carregando discos…");
            });
        } else if volumes.is_empty() {
            ui.label("Nenhum volume encontrado.");
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for v in volumes {
                if let Some(a) = volume_card(ui, v, elevated) {
                    action = Some(a);
                }
                ui.add_space(6.0);
            }
        });
    });
    action
}

fn volume_card(ui: &mut egui::Ui, v: &VolumeInfo, elevated: bool) -> Option<VolumesAction> {
    let mut action = None;
    egui::Frame::group(ui.style())
        .inner_margin(12.0)
        .corner_radius(6.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width().min(720.0));
            ui.horizontal(|ui| {
                // Largura fixa para as barras de todos os cartões ficarem alinhadas.
                let letter =
                    egui::Label::new(RichText::new(format!("{}:", v.letter)).size(28.0).strong());
                ui.add_sized([44.0, 40.0], letter);
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.label(RichText::new(volume_title(v)).strong());
                    let frac = used_fraction(v);
                    let fill = if frac >= 0.9 {
                        Color32::from_rgb(206, 84, 70)
                    } else {
                        Color32::from_rgb(70, 132, 196)
                    };
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .desired_width(320.0)
                            .fill(fill)
                            .text(format!("{:.0}%", frac * 100.0)),
                    );
                    ui.label(RichText::new(usage_text(v)).weak());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(RichText::new("Analisar").strong())
                        .on_hover_text(format!("Ver o mapa de {}:\\", v.letter))
                        .clicked()
                    {
                        action = Some(VolumesAction::Scan(v.letter));
                    }
                    if is_ntfs(v) && !elevated {
                        let b = ui
                            .button("⚡ Varredura rápida (precisa de admin)")
                            .on_hover_text(
                                "Reabre o deixa-seco como administrador e lê a MFT do disco, \
                                 o que costuma ser bem mais rápido.",
                            );
                        if b.clicked() {
                            action = Some(VolumesAction::Elevate(v.letter));
                        }
                    }
                });
            });
        });
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(label: &str, fs: &str, total: u64, free: u64) -> VolumeInfo {
        VolumeInfo {
            letter: 'C',
            label: label.into(),
            fs: fs.into(),
            total,
            free,
        }
    }

    #[test]
    fn used_fraction_handles_edges() {
        assert_eq!(used_fraction(&vol("", "NTFS", 0, 0)), 0.0);
        assert_eq!(used_fraction(&vol("", "NTFS", 100, 25)), 0.75);
        assert_eq!(used_fraction(&vol("", "NTFS", 100, 500)), 0.0);
    }

    #[test]
    fn titles_and_text_pt_br() {
        assert_eq!(volume_title(&vol("", "NTFS", 1, 0)), "Disco local · NTFS");
        assert_eq!(volume_title(&vol("Fotos", "exFAT", 1, 0)), "Fotos · exFAT");
        assert_eq!(
            usage_text(&vol("", "NTFS", 2048, 1024)),
            "1,0 KB usados de 2,0 KB · 1,0 KB livres"
        );
        assert!(is_ntfs(&vol("", "ntfs", 1, 0)));
        assert!(!is_ntfs(&vol("", "FAT32", 1, 0)));
    }
}
