//! Detecting usage of the `#[debugger_visualizer]` attribute.

use hir::CRATE_HIR_ID;
use rustc_data_structures::fx::FxHashSet;
use rustc_expand::base::resolve_path;
use rustc_hir as hir;
use rustc_hir::def_id::CrateNum;
use rustc_hir::HirId;
use rustc_middle::ty::query::Providers;
use rustc_middle::ty::TyCtxt;
use rustc_span::def_id::LOCAL_CRATE;
use rustc_span::{sym, DebuggerVisualizerFile, DebuggerVisualizerType};

use std::sync::Arc;

fn check_for_debugger_visualizer<'tcx>(
    tcx: TyCtxt<'tcx>,
    hir_id: HirId,
    debugger_visualizers: &mut FxHashSet<DebuggerVisualizerFile>,
) {
    let attrs = tcx.hir().attrs(hir_id);
    for attr in attrs {
        if attr.has_name(sym::debugger_visualizer) {
            let list = match attr.meta_item_list() {
                Some(list) => list,
                _ => continue,
            };

            let meta_item = match list.len() {
                1 => match list[0].meta_item() {
                    Some(meta_item) => meta_item,
                    _ => continue,
                },
                _ => continue,
            };

            let file = match (meta_item.name_or_empty(), meta_item.value_str()) {
                (sym::natvis_file, Some(value)) => {
                    match resolve_path(&tcx.sess.parse_sess, value.as_str(), attr.span) {
                        Ok(file) => file,
                        Err(mut err) => {
                            err.emit();
                            continue;
                        }
                    }
                }
                (_, _) => continue,
            };

            if file.is_file() {
                let contents = match std::fs::read(&file) {
                    Ok(contents) => contents,
                    Err(err) => {
                        tcx.sess
                            .struct_span_err(
                                attr.span,
                                &format!(
                                    "Unable to read contents of file `{}`. {}",
                                    file.display(),
                                    err
                                ),
                            )
                            .emit();
                        continue;
                    }
                };

                debugger_visualizers.insert(DebuggerVisualizerFile::new(
                    Arc::from(contents),
                    DebuggerVisualizerType::Natvis,
                ));
            } else {
                tcx.sess
                    .struct_span_err(attr.span, &format!("{} is not a valid file", file.display()))
                    .emit();
            }
        }
    }
}

/// Traverses and collects the debugger visualizers for a specific crate.
fn debugger_visualizers<'tcx>(tcx: TyCtxt<'tcx>, cnum: CrateNum) -> Vec<DebuggerVisualizerFile> {
    assert_eq!(cnum, LOCAL_CRATE);

    // Initialize the collector.
    let mut debugger_visualizers = FxHashSet::default();

    // Collect debugger visualizers in this crate.
    tcx.hir().for_each_module(|id| {
        check_for_debugger_visualizer(
            tcx,
            tcx.hir().local_def_id_to_hir_id(id),
            &mut debugger_visualizers,
        )
    });

    // Collect debugger visualizers on the crate attributes.
    check_for_debugger_visualizer(tcx, CRATE_HIR_ID, &mut debugger_visualizers);

    // Extract out the found debugger_visualizer items.
    let mut visualizers = debugger_visualizers.into_iter().collect::<Vec<_>>();

    // Sort the visualizers so we always get a deterministic query result.
    visualizers.sort();
    visualizers
}

pub fn provide(providers: &mut Providers) {
    providers.debugger_visualizers = debugger_visualizers;
}
