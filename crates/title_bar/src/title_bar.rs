mod application_menu;

mod onboarding_banner;
mod title_bar_settings;
mod update_version;

use crate::application_menu::{
  ActivateDirection, ActivateMenuLeft, ActivateMenuRight, ApplicationMenu, OpenApplicationMenu,
  show_menus,
};
use arrayvec::ArrayVec;
use git_ui_core::worktree_picker::WorktreePicker;
use path::PathStyle;
pub use platform_title_bar::{
  self, DraggedWindowTab, MergeAllWindows, MoveTabToNewWindow, PlatformTitleBar, ShowNextWindowTab,
  ShowPreviousWindowTab,
};
use project::{
  Project, git_store::GitStoreEvent, linked_worktree_short_name, project_settings::ProjectSettings,
  repo_identity_path, repo_identity_path_if_local, trusted_worktrees::TrustedWorktrees,
};

use gpui::{
  Action, Anchor, AnyElement, App, Context, Element, Entity, Focusable, InteractiveElement,
  IntoElement, MouseButton, ParentElement, Render, Styled, Subscription, WeakEntity, Window, div,
};

use settings::Settings as _;

use std::path::Path;
use title_bar_settings::TitleBarSettings;
use ui::{
  ButtonLike, ContextMenu, PopoverMenu, TintColor, Tooltip, prelude::*,
  utils::platform_title_bar_height,
};
use update_version::UpdateVersion;
use util::ResultExt;
use workspace::{AccessibleMode, ToggleWorktreeSecurity, Workspace};

pub use onboarding_banner::restore_banner;

const MAX_PROJECT_NAME_LENGTH: usize = 40;
const MAX_BRANCH_NAME_LENGTH: usize = 40;
const MAX_SHORT_SHA_LENGTH: usize = 8;

fn linked_worktree_name_anchor<'a>(
  main_worktree_path: Option<&'a Path>,
  repository_identity_path: Option<&'a Path>,
  is_linked_worktree: bool,
) -> Option<&'a Path> {
  main_worktree_path.or_else(|| {
    is_linked_worktree
      .then_some(repository_identity_path)
      .flatten()
  })
}

pub fn init(cx: &mut App) {
  platform_title_bar::PlatformTitleBar::init(cx);

  cx.observe_new(|workspace: &mut Workspace, window, cx| {
    let Some(window) = window else {
      return;
    };
    let item = cx.new(|cx| TitleBar::new("title-bar", workspace, window, cx));
    workspace.set_titlebar_item(item.into(), window, cx);

    workspace.register_action(|workspace, action: &OpenApplicationMenu, window, cx| {
      if let Some(titlebar) = workspace
        .titlebar_item()
        .and_then(|item| item.downcast::<TitleBar>().ok())
      {
        titlebar.update(cx, |titlebar, cx| {
          if let Some(ref menu) = titlebar.application_menu {
            menu.update(cx, |menu, cx| menu.open_menu(action, window, cx));
          }
        });
      }
    });

    workspace.register_action(|workspace, _: &ActivateMenuRight, window, cx| {
      if let Some(titlebar) = workspace
        .titlebar_item()
        .and_then(|item| item.downcast::<TitleBar>().ok())
      {
        titlebar.update(cx, |titlebar, cx| {
          if let Some(ref menu) = titlebar.application_menu {
            menu.update(cx, |menu, cx| {
              menu.navigate_menus_in_direction(ActivateDirection::Right, window, cx)
            });
          }
        });
      }
    });

    workspace.register_action(|workspace, _: &ActivateMenuLeft, window, cx| {
      if let Some(titlebar) = workspace
        .titlebar_item()
        .and_then(|item| item.downcast::<TitleBar>().ok())
      {
        titlebar.update(cx, |titlebar, cx| {
          if let Some(ref menu) = titlebar.application_menu {
            menu.update(cx, |menu, cx| {
              menu.navigate_menus_in_direction(ActivateDirection::Left, window, cx)
            });
          }
        });
      }
    });
  })
  .detach();
}

pub struct TitleBar {
  platform_titlebar: Entity<PlatformTitleBar>,
  project: Entity<Project>,
  workspace: WeakEntity<Workspace>,
  application_menu: Option<Entity<ApplicationMenu>>,
  update_version: Entity<UpdateVersion>,
  _subscriptions: Vec<Subscription>,
}

impl Render for TitleBar {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let title_bar_settings = *TitleBarSettings::get_global(cx);
    let button_layout = title_bar_settings.button_layout;
    let is_git_enabled = ProjectSettings::get_global(cx).git.enabled.status;

    let show_menus = show_menus(cx);

    let mut children = <ArrayVec<_, 2>>::new();

    let mut project_name = None;
    let mut repository = None;
    let mut linked_worktree_name = None;
    if let Some(worktree) = self.effective_active_worktree(cx) {
      let path_style = worktree.read(cx).path_style();
      repository = self.get_repository_for_worktree(&worktree, path_style, cx);
      let worktree_abs_path = worktree.read(cx).abs_path();
      project_name = worktree
        .read(cx)
        .root_name()
        .file_name()
        .map(|name| SharedString::from(name.to_string()));
      if let Some(repo) = &repository {
        let repo = repo.read(cx);
        let identity = repo_identity_path(&repo.common_dir_abs_path, path_style);
        let identity_fallback = repo_identity_path_if_local(&repo.common_dir_abs_path, path_style);
        linked_worktree_name = linked_worktree_name_anchor(
          repo.main_worktree_abs_path(),
          identity_fallback,
          repo.is_linked_worktree(),
        )
        .and_then(|name_anchor_path| {
          linked_worktree_short_name(
            name_anchor_path,
            repo.work_directory_abs_path.as_ref(),
            path_style,
          )
        })
        .or_else(|| {
          repo
            .is_linked_worktree()
            .then_some(project_name.clone())
            .flatten()
        });

        let display_name = if identity.extension() == Some(std::ffi::OsStr::new("git")) {
          identity.file_stem()
        } else {
          path_style.file_name(identity)
        };

        if let Some(repo_name) = display_name.and_then(|n| n.to_str()) {
          let visible_worktrees_in_repo =
            self.visible_worktrees_in_repository(repo, path_style, cx);
          let name = if visible_worktrees_in_repo == 1 {
            if let Ok(relative) =
              path_style.strip_prefix(&worktree_abs_path, &*repo.work_directory_abs_path)
            {
              if relative.as_os_str().is_empty() {
                repo_name.to_string()
              } else {
                format!(
                  "{}{}{}",
                  repo_name,
                  path_style.primary_separator(),
                  relative.display()
                )
              }
            } else {
              repo_name.to_string()
            }
          } else {
            repo_name.to_string()
          };
          project_name = Some(SharedString::from(name));
        }
      }
    }

    children.push(
      h_flex()
        .h_full()
        .gap_0p5()
        .map(|title_bar| {
          let mut render_project_items =
            title_bar_settings.show_branch_name || title_bar_settings.show_project_items;
          title_bar
            .when_some(
              self.application_menu.clone().filter(|_| !show_menus),
              |title_bar, menu| {
                // Hide the project/branch items to make room when the
                // menu bar is expanded -- except in accessible mode,
                // where the menu bar is always expanded but those
                // controls must still remain reachable.
                render_project_items &=
                  !menu.update(cx, |menu, cx| menu.all_menus_shown(cx)) || cx.accessible_mode();
                title_bar.child(menu)
              },
            )
            .children(self.render_restricted_mode(cx))
            .when(render_project_items, |title_bar| {
              title_bar
                .when(title_bar_settings.show_project_items, |title_bar| {
                  title_bar.child(self.render_project_name(project_name, window, cx))
                })
                .when_some(
                  repository.filter(|_| is_git_enabled),
                  |title_bar, repository| {
                    title_bar.children(self.render_worktree_and_branch(
                      repository,
                      linked_worktree_name,
                      cx,
                    ))
                  },
                )
            })
        })
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .into_any_element(),
    );

    children.push(
      h_flex()
        .pr_1()
        .gap_1()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(self.update_version.clone())
        .when(title_bar_settings.show_user_menu, |this| {
          this.child(self.render_user_menu_button(cx))
        })
        .into_any_element(),
    );

    if show_menus {
      self.platform_titlebar.update(cx, |this, _| {
        this.set_button_layout(button_layout);
        this.set_children(
          self
            .application_menu
            .clone()
            .map(|menu| menu.into_any_element()),
        );
      });

      let height = platform_title_bar_height(window);
      let title_bar_color = self.platform_titlebar.update(cx, |platform_titlebar, cx| {
        platform_titlebar.title_bar_color(window, cx)
      });

      v_flex()
        .w_full()
        .child(self.platform_titlebar.clone().into_any_element())
        .child(
          h_flex()
            .bg(title_bar_color)
            .h(height)
            .pl_2()
            .justify_between()
            .w_full()
            .children(children),
        )
        .into_any_element()
    } else {
      self.platform_titlebar.update(cx, |this, _| {
        this.set_button_layout(button_layout);
        this.set_children(children);
      });
      self.platform_titlebar.clone().into_any_element()
    }
  }
}

impl TitleBar {
  pub fn new(
    id: impl Into<ElementId>,
    workspace: &Workspace,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let project = workspace.project().clone();
    let git_store = project.read(cx).git_store().clone();

    let platform_style = PlatformStyle::platform();
    let application_menu = match platform_style {
      PlatformStyle::Mac => {
        if option_env!("ZED_USE_CROSS_PLATFORM_MENU").is_some() {
          Some(cx.new(|cx| ApplicationMenu::new(window, cx)))
        } else {
          None
        }
      }
      PlatformStyle::Linux | PlatformStyle::Windows => {
        Some(cx.new(|cx| ApplicationMenu::new(window, cx)))
      }
    };

    let mut subscriptions = Vec::new();
    subscriptions.push(
      cx.observe(&workspace.weak_handle().upgrade().unwrap(), |_, _, cx| {
        cx.notify()
      }),
    );

    subscriptions.push(
      cx.subscribe(&git_store, move |_, _, event, cx| match event {
        GitStoreEvent::ActiveRepositoryChanged(_)
        | GitStoreEvent::RepositoryUpdated(_, _, true) => {
          cx.notify();
        }
        _ => {}
      }),
    );

    if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
      subscriptions.push(
        cx.subscribe(&workspace_entity, |_, _, event: &workspace::Event, cx| {
          if matches!(event, workspace::Event::WorktreeCreationChanged) {
            cx.notify();
          }
        }),
      );
    }
    subscriptions.push(cx.observe_button_layout_changed(window, |_, _, cx| cx.notify()));
    if let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) {
      subscriptions.push(cx.subscribe(&trusted_worktrees, |_, _, _, cx| {
        cx.notify();
      }));
    }

    let update_version = cx.new(|cx| UpdateVersion::new(cx));
    let platform_titlebar = cx.new(|cx| PlatformTitleBar::new(id, cx));

    Self {
      platform_titlebar,
      project,
      workspace: workspace.weak_handle(),
      application_menu,
      update_version,
      _subscriptions: subscriptions,
    }
  }

  fn worktree_count(&self, cx: &App) -> usize {
    self.project.read(cx).visible_worktrees(cx).count()
  }

  /// Returns the worktree to display in the title bar.
  /// - Prefer the worktree owning the project's active repository
  /// - Fall back to the first visible worktree
  pub fn effective_active_worktree(&self, cx: &App) -> Option<Entity<project::Worktree>> {
    let project = self.project.read(cx);

    if let Some(repo) = project.active_repository(cx) {
      let repo = repo.read(cx);
      let repo_path = &repo.work_directory_abs_path;

      for worktree in project.visible_worktrees(cx) {
        let worktree_path = worktree.read(cx).abs_path();
        if worktree_path == *repo_path || worktree_path.starts_with(repo_path.as_ref()) {
          return Some(worktree);
        }
      }
    }

    project.visible_worktrees(cx).next()
  }

  fn get_repository_for_worktree(
    &self,
    worktree: &Entity<project::Worktree>,
    path_style: PathStyle,
    cx: &App,
  ) -> Option<Entity<project::git_store::Repository>> {
    let project = self.project.read(cx);
    let git_store = project.git_store().read(cx);
    let worktree_path = worktree.read(cx).abs_path();

    git_store
      .repositories()
      .values()
      .filter(|repo| {
        let repo_path = &repo.read(cx).work_directory_abs_path;
        worktree_path == *repo_path || path_style.starts_with(&worktree_path, repo_path.as_ref())
      })
      .max_by_key(|repo| repo.read(cx).work_directory_abs_path.as_os_str().len())
      .cloned()
  }

  fn visible_worktrees_in_repository(
    &self,
    repository: &project::git_store::Repository,
    path_style: PathStyle,
    cx: &App,
  ) -> usize {
    let repo_path = &repository.work_directory_abs_path;
    self
      .project
      .read(cx)
      .visible_worktrees(cx)
      .filter(|worktree| {
        let worktree_path = worktree.read(cx).abs_path();
        worktree_path == *repo_path || path_style.starts_with(&worktree_path, repo_path.as_ref())
      })
      .count()
  }

  pub fn render_restricted_mode(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
    let has_restricted_worktrees =
      TrustedWorktrees::has_restricted_worktrees(&self.project.read(cx).worktree_store(), cx);
    if !has_restricted_worktrees {
      return None;
    }

    let button = Button::new("restricted_mode_trigger", "Restricted Mode")
      .style(ButtonStyle::Tinted(TintColor::Warning))
      .label_size(LabelSize::Small)
      .color(Color::Warning)
      .start_icon(
        Icon::new(IconName::Warning)
          .size(IconSize::Small)
          .color(Color::Warning),
      )
      .tooltip(|_, cx| {
        Tooltip::with_meta(
          "You're in Restricted Mode",
          Some(&ToggleWorktreeSecurity),
          "Mark this project as trusted and unlock all features",
          cx,
        )
      })
      .on_click({
        cx.listener(move |this, _, window, cx| {
          this
            .workspace
            .update(cx, |workspace, cx| {
              workspace.show_worktree_trust_security_modal(true, window, cx)
            })
            .log_err();
        })
      });

    if ui::utils::MACOS_SDK_26_OR_LATER {
      // Make up for Tahoe's traffic light buttons having less spacing around them
      Some(div().child(button).ml_0p5().into_any_element())
    } else {
      Some(button.into_any_element())
    }
  }

  fn render_project_name(
    &self,
    name: Option<SharedString>,
    _: &mut Window,
    cx: &mut Context<Self>,
  ) -> impl IntoElement {
    let workspace = self.workspace.clone();

    let is_project_selected = name.is_some();

    let display_name = if let Some(ref name) = name {
      util::truncate_and_trailoff(name, MAX_PROJECT_NAME_LENGTH)
    } else {
      "Open Recent Project".to_string()
    };

    let focus_handle = workspace
      .upgrade()
      .map(|w| w.read(cx).focus_handle(cx))
      .unwrap_or_else(|| cx.focus_handle());

    PopoverMenu::new("recent-projects-menu")
      .menu(move |window, cx| {
        Some(recent_projects::RecentProjects::popover(
          workspace.clone(),
          Vec::new(),
          None,
          focus_handle.clone(),
          window,
          cx,
        ))
      })
      .trigger_with_tooltip(
        Button::new("project_name_trigger", display_name)
          .label_size(LabelSize::Small)
          .tab_index(0isize)
          .when(self.worktree_count(cx) > 1, |this| {
            this.end_icon(
              Icon::new(IconName::ChevronDown)
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
          })
          .selected_style(ButtonStyle::Tinted(TintColor::Accent))
          .when(!is_project_selected, |s| s.color(Color::Muted)),
        move |_window, cx| {
          Tooltip::for_action("Recent Projects", &zed_actions::OpenRecent::default(), cx)
        },
      )
      .anchor(gpui::Anchor::TopLeft)
      .into_any_element()
  }

  fn render_worktree_and_branch(
    &self,
    repository: Entity<project::git_store::Repository>,
    linked_worktree_name: Option<SharedString>,
    cx: &mut Context<Self>,
  ) -> Option<AnyElement> {
    let workspace = self.workspace.upgrade()?;

    let (branch_name, icon_info, is_detached_head) = {
      let repo = repository.read(cx);

      let is_detached_head = repo.branch.is_none();

      let branch_name = repo
        .branch
        .as_ref()
        .map(|branch| branch.name())
        .map(|name| util::truncate_and_trailoff(name, MAX_BRANCH_NAME_LENGTH))
        .or_else(|| {
          repo.head_commit.as_ref().map(|commit| {
            commit
              .sha
              .chars()
              .take(MAX_SHORT_SHA_LENGTH)
              .collect::<String>()
          })
        });

      let status = repo.status_summary();
      let tracked = status.index + status.worktree;
      let icon_info = if status.conflict > 0 {
        (IconName::Warning, Color::VersionControlConflict)
      } else if tracked.modified > 0 {
        (IconName::SquareDot, Color::VersionControlModified)
      } else if tracked.added > 0 || status.untracked > 0 {
        (IconName::SquarePlus, Color::VersionControlAdded)
      } else if tracked.deleted > 0 {
        (IconName::SquareMinus, Color::VersionControlDeleted)
      } else {
        (IconName::GitBranch, Color::Muted)
      };

      (branch_name, icon_info, is_detached_head)
    };

    let settings = TitleBarSettings::get_global(cx);
    let effective_repository = Some(repository);

    let worktree_label: SharedString = linked_worktree_name.unwrap_or_else(|| "main".into());

    let (creation_in_progress, is_switch) = {
      let creation = workspace.read(cx).active_worktree_creation();
      (creation.label.clone(), creation.is_switch)
    };
    let is_creating = creation_in_progress.is_some();

    let display_label: SharedString = if let Some(ref name) = creation_in_progress {
      if is_switch {
        format!("Loading {}…", name).into()
      } else {
        format!("Creating {}…", name).into()
      }
    } else {
      worktree_label.clone()
    };

    let worktree_button = settings.show_worktree_name.then(|| {
      let project = self.project.clone();
      let workspace_handle = workspace.downgrade();
      PopoverMenu::new("worktree-picker-menu")
        .menu(move |window, cx| {
          // When opened from the title bar, focus is on the trigger
          // button (not a dock), so `focused_dock` is `None`. That's
          // fine — there's no prior dock focus to restore.
          Some(
            cx.new(|cx| WorktreePicker::new(project.clone(), workspace_handle.clone(), window, cx)),
          )
        })
        .trigger_with_tooltip(
          Button::new("worktree_picker_trigger", display_label)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .tab_index(0isize)
            .loading(is_creating)
            .start_icon(
              Icon::new(IconName::GitWorktree)
                .size(IconSize::XSmall)
                .color(Color::Muted),
            ),
          move |_window, cx| {
            Tooltip::with_meta(
              "Worktree",
              Some(&zed_actions::git::Worktree),
              format!("Currently In Use: {}", worktree_label),
              cx,
            )
          },
        )
        .anchor(gpui::Anchor::TopLeft)
    });

    let branch_picker = branch_name.and_then(|branch_name| {
      settings.show_branch_name.then(|| {
        let branch_tooltip_label = branch_name.clone();
        let (branch_icon, branch_icon_color) = if settings.show_branch_status_icon {
          icon_info
        } else {
          (IconName::GitBranch, Color::Muted)
        };

        let trigger = if is_detached_head {
          Button::new("project_branch_trigger", "Create Branch")
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .label_size(LabelSize::Small)
            .tab_index(0isize)
            .start_icon(
              Icon::new(IconName::GitBranchPlus)
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
        } else {
          Button::new("project_branch_trigger", branch_name)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .label_size(LabelSize::Small)
            .color(Color::Muted)
            .tab_index(0isize)
            .start_icon(
              Icon::new(branch_icon)
                .size(IconSize::XSmall)
                .color(branch_icon_color),
            )
        };

        PopoverMenu::new("branch-menu")
          .menu(move |window, cx| {
            git_ui_core::build_branch_picker(
              workspace.downgrade(),
              effective_repository.clone(),
              window,
              cx,
            )
          })
          .trigger_with_tooltip(trigger, move |_window, cx| {
            let meta = if is_detached_head {
              format!("Detached HEAD: {}", branch_tooltip_label)
            } else {
              format!("Currently Checked Out: {}", branch_tooltip_label)
            };
            Tooltip::with_meta("Branch & Stash", Some(&zed_actions::git::Branch), meta, cx)
          })
          .anchor(gpui::Anchor::TopLeft)
      })
    });

    if worktree_button.is_none() && branch_picker.is_none() {
      return None;
    }

    let show_separator = worktree_button.is_some() && branch_picker.is_some();

    Some(
      h_flex()
        .gap_px()
        .children(worktree_button)
        .when(show_separator, |this| {
          this.child(
            Label::new("/")
              .size(LabelSize::Small)
              .color(Color::Muted)
              .alpha(0.25),
          )
        })
        .children(branch_picker)
        .into_any_element(),
    )
  }

  pub fn render_user_menu_button(&mut self, cx: &mut Context<Self>) -> impl Element {
    let show_update_button = self.update_version.read(cx).show_update_in_menu_bar();

    let trigger = ButtonLike::new("user-menu")
      .aria_label("User menu")
      .tab_index(0isize)
      .child(Icon::new(IconName::ChevronDown).size(IconSize::Small));

    PopoverMenu::new("user-menu")
      .trigger(trigger)
      .menu(move |window, cx| {
        ContextMenu::build(window, cx, |menu, _, _cx| {
          menu
            .when(show_update_button, |this| {
              this
                .custom_entry(
                  move |_window, _cx| {
                    h_flex()
                      .w_full()
                      .gap_1()
                      .justify_between()
                      .child(Label::new("Restart to update Zed").color(Color::Accent))
                      .child(
                        Icon::new(IconName::Download)
                          .size(IconSize::Small)
                          .color(Color::Accent),
                      )
                      .into_any_element()
                  },
                  move |_, cx| {
                    workspace::reload(cx);
                  },
                )
                .separator()
            })
            .action("Settings", zed_actions::OpenSettings.boxed_clone())
            .action("Keymap", Box::new(zed_actions::OpenKeymap))
            .action(
              "Themes…",
              zed_actions::theme_selector::Toggle::default().boxed_clone(),
            )
            .action(
              "Icon Themes…",
              zed_actions::icon_theme_selector::Toggle::default().boxed_clone(),
            )
            .action(
              "Extensions",
              zed_actions::Extensions::default().boxed_clone(),
            )
        })
        .into()
      })
      .anchor(Anchor::TopRight)
  }
}