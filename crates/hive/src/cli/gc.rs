//! `hive gc`: the collector by hand (`run`), the exemption (`keep`) and
//! the way back out of the trash (`restore`). The logic is `crate::gc`;
//! this prints.

use super::util::fail;
use crate::gc;

pub(crate) fn run_cmd(dry_run: bool, json: bool) {
    let mode = if dry_run {
        gc::Mode::DryRun
    } else {
        gc::Mode::Manual
    };
    match gc::run(mode) {
        Ok(report) => {
            if json {
                println!("{}", gc::render_json(&report));
            } else {
                print!("{}", gc::render_text(&report));
            }
        }
        Err(e) => fail(&format!("gc: {e}")),
    }
}

pub(crate) fn keep_cmd(target: &str, off: bool) {
    match gc::set_keep(target, !off) {
        Ok(line) => println!("{line}"),
        Err(e) => fail(&e.to_string()),
    }
}

pub(crate) fn restore_cmd(archive_id: &str, as_name: &str) {
    let as_name = (!as_name.is_empty()).then_some(as_name);
    match gc::restore_archive(archive_id, as_name) {
        Ok(restored) => {
            println!(
                "restored archive {} as team '{}' at {}",
                restored.archive.id,
                restored.team,
                restored.dir.display()
            );
            if restored.team != restored.archive.team {
                println!(
                    "note: absolute paths recorded under the old name ({}) are not rewritten",
                    restored.archive.team
                );
            }
            println!("next: hive attach {}", restored.team);
        }
        Err(e) => fail(&e.to_string()),
    }
}
