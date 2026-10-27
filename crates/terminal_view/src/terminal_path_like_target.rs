use super::{HoverTarget, HoveredWord, TerminalView};
use anyhow::Result;
use editor::items::open_resolved_target;
use gpui::{Context, Task, TaskExt, WeakEntity, Window};
use std::path::PathBuf;
use terminal::PathLikeTarget;
use workspace::path_link::PathMatching;
use workspace::path_link::resolve_open_target;

use workspace::{Workspace, path_link::OpenTarget};

pub(super) fn hover_path_like_target(
  workspace: &WeakEntity<Workspace>,
  hovered_word: HoveredWord,
  path_like_target: &PathLikeTarget,
  cx: &mut Context<TerminalView>,
) -> Task<()> {
  possible_hover_target(workspace, hovered_word, path_like_target, cx)
}

fn possible_hover_target(
  workspace: &WeakEntity<Workspace>,
  hovered_word: HoveredWord,
  path_like_target: &PathLikeTarget,
  cx: &mut Context<TerminalView>,
) -> Task<()> {
  let file_to_open_task = resolve_open_target(
    workspace,
    PathMatching::Heuristic,
    &path_like_target.maybe_path,
    path_like_target.working_directory.as_deref(),
    cx,
  );

  cx.spawn(async move |terminal_view, cx| {
    let file_to_open = file_to_open_task.await;
    terminal_view
      .update(cx, |terminal_view, cx| {
        match file_to_open {
          Some(OpenTarget::Path(path, ..) | OpenTarget::Worktree(path, ..)) => {
            terminal_view.hover = Some(HoverTarget {
              tooltip: path.to_string(&|path: &PathBuf| path.to_string_lossy().into_owned()),
              hovered_word,
            });
          }
          None => {
            terminal_view.hover = None;
          }
        };
        cx.notify();
      })
      .ok();
  })
}

pub(super) fn open_path_like_target(
  workspace: &WeakEntity<Workspace>,
  terminal_view: &mut TerminalView,
  path_like_target: &PathLikeTarget,
  window: &mut Window,
  cx: &mut Context<TerminalView>,
) {
  {
    possibly_open_target(workspace, terminal_view, path_like_target, window, cx)
      .detach_and_log_err(cx)
  }
}

fn possibly_open_target(
  workspace: &WeakEntity<Workspace>,
  terminal_view: &mut TerminalView,
  path_like_target: &PathLikeTarget,
  window: &mut Window,
  cx: &mut Context<TerminalView>,
) -> Task<Result<Option<OpenTarget>>> {
  if terminal_view.hover.is_none() {
    return Task::ready(Ok(None));
  }
  let workspace = workspace.clone();
  let path_like_target = path_like_target.clone();
  cx.spawn_in(window, async move |terminal_view, cx| {
    let Some(open_target) = terminal_view
      .update(cx, |_, cx| {
        {
          resolve_open_target(
            &workspace,
            PathMatching::Heuristic,
            &path_like_target.maybe_path,
            path_like_target.working_directory.as_deref(),
            cx,
          )
        }
      })?
      .await
    else {
      return Ok(None);
    };

    let opened = open_resolved_target(&workspace, &open_target, cx).await?;
    Ok(opened.then_some(open_target))
  })
}
