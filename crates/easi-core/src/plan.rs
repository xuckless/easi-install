//! Declarative install plan table (`Format × AppKind → [PlanOp]`) and DE refresh table.

use crate::model::{AppKind, Format};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanOp {
    CopyAppImage,
    ExtractAppImageAssets,
    ExtractArchive,
    WireIcons,
    WireDesktop,
    WireBinSymlink,
    RefreshDesktop,
    WriteManifest,
}

const APPIMAGE_GUI_OPS: &[PlanOp] = &[
    PlanOp::CopyAppImage,
    PlanOp::ExtractAppImageAssets,
    PlanOp::WireIcons,
    PlanOp::WireDesktop,
    PlanOp::WireBinSymlink,
    PlanOp::RefreshDesktop,
    PlanOp::WriteManifest,
];

const APPIMAGE_TERM_OPS: &[PlanOp] = &[
    PlanOp::CopyAppImage,
    PlanOp::WireBinSymlink,
    PlanOp::WriteManifest,
];

const ARCHIVE_GUI_OPS: &[PlanOp] = &[
    PlanOp::ExtractArchive,
    PlanOp::WireIcons,
    PlanOp::WireDesktop,
    PlanOp::WireBinSymlink,
    PlanOp::RefreshDesktop,
    PlanOp::WriteManifest,
];

const ARCHIVE_TERM_OPS: &[PlanOp] = &[
    PlanOp::ExtractArchive,
    PlanOp::WireBinSymlink,
    PlanOp::WriteManifest,
];

struct PlanRow {
    format: Format,
    kind: AppKind,
    ops: &'static [PlanOp],
}

const PLAN_TABLE: &[PlanRow] = &[
    PlanRow {
        format: Format::AppImageType1,
        kind: AppKind::Gui,
        ops: APPIMAGE_GUI_OPS,
    },
    PlanRow {
        format: Format::AppImageType1,
        kind: AppKind::Terminal,
        ops: APPIMAGE_TERM_OPS,
    },
    PlanRow {
        format: Format::AppImageType2,
        kind: AppKind::Gui,
        ops: APPIMAGE_GUI_OPS,
    },
    PlanRow {
        format: Format::AppImageType2,
        kind: AppKind::Terminal,
        ops: APPIMAGE_TERM_OPS,
    },
    PlanRow {
        format: Format::TarGz,
        kind: AppKind::Gui,
        ops: ARCHIVE_GUI_OPS,
    },
    PlanRow {
        format: Format::TarGz,
        kind: AppKind::Terminal,
        ops: ARCHIVE_TERM_OPS,
    },
    PlanRow {
        format: Format::TarXz,
        kind: AppKind::Gui,
        ops: ARCHIVE_GUI_OPS,
    },
    PlanRow {
        format: Format::TarXz,
        kind: AppKind::Terminal,
        ops: ARCHIVE_TERM_OPS,
    },
    PlanRow {
        format: Format::TarZst,
        kind: AppKind::Gui,
        ops: ARCHIVE_GUI_OPS,
    },
    PlanRow {
        format: Format::TarZst,
        kind: AppKind::Terminal,
        ops: ARCHIVE_TERM_OPS,
    },
    PlanRow {
        format: Format::TarBz2,
        kind: AppKind::Gui,
        ops: ARCHIVE_GUI_OPS,
    },
    PlanRow {
        format: Format::TarBz2,
        kind: AppKind::Terminal,
        ops: ARCHIVE_TERM_OPS,
    },
    PlanRow {
        format: Format::Zip,
        kind: AppKind::Gui,
        ops: ARCHIVE_GUI_OPS,
    },
    PlanRow {
        format: Format::Zip,
        kind: AppKind::Terminal,
        ops: ARCHIVE_TERM_OPS,
    },
];

pub fn plan_ops(format: Format, kind: AppKind) -> &'static [PlanOp] {
    for row in PLAN_TABLE {
        if row.format == format && row.kind == kind {
            return row.ops;
        }
    }
    ARCHIVE_TERM_OPS
}

/// Desktop environment cache-refresh quirks table.
pub struct DeQuirk {
    pub env_needle: &'static str,
    pub name: &'static str,
    pub refresh_kde_sycoca: bool,
    pub refresh_gtk_icon_cache: bool,
    pub refresh_desktop_db: bool,
}

pub const DE_QUIRKS: &[DeQuirk] = &[
    DeQuirk {
        env_needle: "GNOME",
        name: "GNOME",
        refresh_kde_sycoca: false,
        refresh_gtk_icon_cache: true,
        refresh_desktop_db: true,
    },
    DeQuirk {
        env_needle: "KDE",
        name: "KDE Plasma",
        refresh_kde_sycoca: true,
        refresh_gtk_icon_cache: true,
        refresh_desktop_db: true,
    },
    DeQuirk {
        env_needle: "PLASMA",
        name: "KDE Plasma",
        refresh_kde_sycoca: true,
        refresh_gtk_icon_cache: true,
        refresh_desktop_db: true,
    },
];

pub const GENERIC_DE: DeQuirk = DeQuirk {
    env_needle: "",
    name: "Generic",
    refresh_kde_sycoca: false,
    refresh_gtk_icon_cache: true,
    refresh_desktop_db: true,
};

pub fn detect_de_quirk(xdg_current_desktop: &str) -> &'static DeQuirk {
    let upper = xdg_current_desktop.to_ascii_uppercase();
    for quirk in DE_QUIRKS {
        if upper.contains(quirk.env_needle) {
            return quirk;
        }
    }
    &GENERIC_DE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_table_covers_all_formats_and_kinds() {
        let formats = [
            Format::AppImageType1,
            Format::AppImageType2,
            Format::TarGz,
            Format::TarXz,
            Format::TarZst,
            Format::TarBz2,
            Format::Zip,
        ];
        for fmt in formats {
            let gui = plan_ops(fmt, AppKind::Gui);
            assert!(gui.contains(&PlanOp::WireDesktop));
            assert!(gui.contains(&PlanOp::WriteManifest));
            let term = plan_ops(fmt, AppKind::Terminal);
            assert!(!term.contains(&PlanOp::WireDesktop));
            assert!(term.contains(&PlanOp::WireBinSymlink));
        }
    }

    #[test]
    fn de_quirk_table_matches_gnome_and_kde() {
        let gnome = detect_de_quirk("ubuntu:GNOME");
        assert_eq!(gnome.name, "GNOME");
        assert!(gnome.refresh_desktop_db);
        assert!(gnome.refresh_gtk_icon_cache);
        assert!(!gnome.refresh_kde_sycoca);

        let kde = detect_de_quirk("KDE");
        assert_eq!(kde.name, "KDE Plasma");
        assert!(kde.refresh_kde_sycoca);

        let generic = detect_de_quirk("sway");
        assert_eq!(generic.name, "Generic");
    }
}
