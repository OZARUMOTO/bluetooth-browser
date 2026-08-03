// SPDX-FileCopyrightText: 2026 Michael Totten <mike@ozaru.io>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::Result;

fn main() -> Result<()> {
    slint_keyos_platform_build::compile_options(slint_keyos_platform_build::CompileOptions {
        module_path: "ui/app.slint",
        include_router: true,
        include_slint: true,
        include_translations: false,
        include_time_localization: false,
    });
    Ok(())
}
