use crate::components::KernelListItem;
use crate::kernels::RemoteRunningKernel;
use crate::setup_editor_session_actions;
use crate::{
    kernels::{Kernel, KernelSpecification, NativeRunningKernel},
    outputs::{ExecutionStatus, ExecutionView},
    KernelStatus,
};
use collections::{HashMap, HashSet};
use editor::{
    display_map::{
        BlockContext, BlockId, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId,
        RenderBlock,
    },
    scroll::Autoscroll,
    Anchor, AnchorRangeExt as _, Editor, MultiBuffer, ToPoint,
};
use futures::FutureExt as _;
use gpui::{
    div, prelude::*, EventEmitter, Model, ModelContext, Render, Subscription, Task, WeakModel,
    Window,
};
use language::Point;
use project::Fs;
use runtimelib::{
    ExecuteRequest, ExecutionState, InterruptRequest, JupyterMessage, JupyterMessageContent,
    ShutdownRequest,
};
use std::{env::temp_dir, ops::Range, sync::Arc, time::Duration};
use theme::ActiveTheme;
use ui::{prelude::*, IconButtonShape, Tooltip};
use util::ResultExt as _;

pub struct Session {
    fs: Arc<dyn Fs>,
    editor: WeakModel<Editor>,
    pub kernel: Kernel,
    blocks: HashMap<String, EditorBlock>,
    pub kernel_specification: KernelSpecification,
    _buffer_subscription: Subscription,
}

struct EditorBlock {
    code_range: Range<Anchor>,
    invalidation_anchor: Anchor,
    block_id: CustomBlockId,
    execution_view: Model<ExecutionView>,
}

type CloseBlockFn =
    Arc<dyn for<'a> Fn(CustomBlockId, &'a mut Window, &mut AppContext) + Send + Sync + 'static>;

impl EditorBlock {
    fn new(
        editor: WeakModel<Editor>,
        code_range: Range<Anchor>,
        status: ExecutionStatus,
        on_close: CloseBlockFn,
        window: &mut Window,
        cx: &mut ModelContext<Session>,
    ) -> anyhow::Result<Self> {
        let editor = editor
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("editor is not open"))?;
        let workspace = editor
            .read(cx)
            .workspace()
            .ok_or_else(|| anyhow::anyhow!("workspace dropped"))?;

        let execution_view = window.new_view(cx, |window, cx| {
            ExecutionView::new(status, workspace.downgrade(), window, cx)
        });

        let (block_id, invalidation_anchor) = editor.update(cx, |editor, cx| {
            let buffer = editor.buffer().clone();
            let buffer_snapshot = buffer.read(cx).snapshot(cx);
            let end_point = code_range.end.to_point(&buffer_snapshot);
            let next_row_start = end_point + Point::new(1, 0);
            if next_row_start > buffer_snapshot.max_point() {
                buffer.update(cx, |buffer, cx| {
                    buffer.edit(
                        [(
                            buffer_snapshot.max_point()..buffer_snapshot.max_point(),
                            "\n",
                        )],
                        None,
                        cx,
                    )
                });
            }

            let invalidation_anchor = buffer.read(cx).read(cx).anchor_before(next_row_start);
            let block = BlockProperties {
                placement: BlockPlacement::Below(code_range.end),
                // Take up at least one height for status, allow the editor to determine the real height based on the content from render
                height: 1,
                style: BlockStyle::Sticky,
                render: Self::create_output_area_renderer(execution_view.clone(), on_close.clone()),
                priority: 0,
            };

            let block_id = editor.insert_blocks([block], None, window, cx)[0];
            (block_id, invalidation_anchor)
        });

        anyhow::Ok(Self {
            code_range,
            invalidation_anchor,
            block_id,
            execution_view,
        })
    }

    fn handle_message(
        &mut self,
        message: &JupyterMessage,
        window: &mut Window,
        cx: &mut ModelContext<Session>,
    ) {
        self.execution_view.update(cx, |execution_view, cx| {
            execution_view.push_message(&message.content, window, cx);
        });
    }

    fn create_output_area_renderer(
        execution_view: Model<ExecutionView>,
        on_close: CloseBlockFn,
    ) -> RenderBlock {
        Arc::new(move |cx: &mut BlockContext| {
            let execution_view = execution_view.clone();
            let text_style = crate::outputs::plain::text_style(cx.window, cx.context);

            let gutter = cx.gutter_dimensions;

            let block_id = cx.block_id;
            let on_close = on_close.clone();

            let rem_size = cx.window.rem_size();

            let text_line_height = text_style.line_height_in_pixels(rem_size);

            let close_button = h_flex()
                .flex_none()
                .items_center()
                .justify_center()
                .absolute()
                .top(text_line_height / 2.)
                .right(
                    // 2px is a magic number to nudge the button just a bit closer to
                    // the line number start
                    gutter.full_width() / 2.0 - text_line_height / 2.0 - px(2.),
                )
                .w(text_line_height)
                .h(text_line_height)
                .child(
                    IconButton::new("close_output_area", IconName::Close)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .size(ButtonSize::Compact)
                        .shape(IconButtonShape::Square)
                        .tooltip(|window, cx| Tooltip::text("Close output area", window, cx))
                        .on_click(move |_, window, cx| {
                            if let BlockId::Custom(block_id) = block_id {
                                (on_close)(block_id, window, cx)
                            }
                        }),
                );

            div()
                .id(cx.block_id)
                .block_mouse_down()
                .flex()
                .items_start()
                .min_h(text_line_height)
                .w_full()
                .border_y_1()
                .border_color(cx.theme().colors().border)
                .bg(cx.theme().colors().background)
                .child(
                    div()
                        .relative()
                        .w(gutter.full_width())
                        .h(text_line_height * 2)
                        .child(close_button),
                )
                .child(
                    div()
                        .flex_1()
                        .size_full()
                        .py(text_line_height / 2.)
                        .mr(gutter.width)
                        .child(execution_view),
                )
                .into_any_element()
        })
    }
}

impl Session {
    pub fn new(
        editor: WeakModel<Editor>,
        fs: Arc<dyn Fs>,
        kernel_specification: KernelSpecification,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) -> Self {
        let subscription = match editor.upgrade() {
            Some(editor) => {
                let buffer = editor.read(cx).buffer().clone();
                cx.subscribe_in(&buffer, window, Self::on_buffer_event)
            }
            None => Subscription::new(|| {}),
        };

        let editor_handle = editor.clone();

        editor
            .update(cx, |editor, _cx| {
                setup_editor_session_actions(editor, editor_handle);
            })
            .ok();

        let mut session = Self {
            fs,
            editor,
            kernel: Kernel::StartingKernel(Task::ready(()).shared()),
            blocks: HashMap::default(),
            kernel_specification,
            _buffer_subscription: subscription,
        };

        session.start_kernel(window, cx);
        session
    }

    fn start_kernel(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) {
        let kernel_language = self.kernel_specification.language();
        let entity_id = self.editor.entity_id();
        let working_directory = self
            .editor
            .upgrade()
            .and_then(|editor| editor.read(cx).working_directory(cx))
            .unwrap_or_else(temp_dir);

        telemetry::event!(
            "Kernel Status Changed",
            kernel_language,
            kernel_status = KernelStatus::Starting.to_string(),
            repl_session_id = cx.entity_id().to_string(),
        );

        let session_view = cx.model().clone();

        let kernel = match self.kernel_specification.clone() {
            KernelSpecification::Jupyter(kernel_specification)
            | KernelSpecification::PythonEnv(kernel_specification) => NativeRunningKernel::new(
                kernel_specification,
                entity_id,
                working_directory,
                self.fs.clone(),
                session_view,
                window,
                cx,
            ),
            KernelSpecification::Remote(remote_kernel_specification) => RemoteRunningKernel::new(
                remote_kernel_specification,
                working_directory,
                session_view,
                window,
                cx,
            ),
        };

        let pending_kernel = cx
            .spawn_in(window, |this, mut cx| async move {
                let kernel = kernel.await;

                match kernel {
                    Ok(kernel) => {
                        this.update_in(&mut cx, |session, window, cx| {
                            session.kernel(Kernel::RunningKernel(kernel), window, cx);
                        })
                        .ok();
                    }
                    Err(err) => {
                        this.update_in(&mut cx, |session, window, cx| {
                            session.kernel_errored(err.to_string(), window, cx);
                        })
                        .ok();
                    }
                }
            })
            .shared();

        self.kernel(Kernel::StartingKernel(pending_kernel), window, cx);
        cx.notify();
    }

    pub fn kernel_errored(
        &mut self,
        error_message: String,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        self.kernel(Kernel::ErroredLaunch(error_message.clone()), window, cx);

        self.blocks.values().for_each(|block| {
            block.execution_view.update(cx, |execution_view, cx| {
                match execution_view.status {
                    ExecutionStatus::Finished => {
                        // Do nothing when the output was good
                    }
                    _ => {
                        // All other cases, set the status to errored
                        execution_view.status =
                            ExecutionStatus::KernelErrored(error_message.clone())
                    }
                }
                cx.notify();
            });
        });
    }

    fn on_buffer_event(
        &mut self,
        buffer: &Model<MultiBuffer>,
        event: &multi_buffer::Event,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        if let multi_buffer::Event::Edited { .. } = event {
            let snapshot = buffer.read(cx).snapshot(cx);

            let mut blocks_to_remove: HashSet<CustomBlockId> = HashSet::default();

            self.blocks.retain(|_id, block| {
                if block.invalidation_anchor.is_valid(&snapshot) {
                    true
                } else {
                    blocks_to_remove.insert(block.block_id);
                    false
                }
            });

            if !blocks_to_remove.is_empty() {
                self.editor
                    .update(cx, |editor, cx| {
                        editor.remove_blocks(blocks_to_remove, None, window, cx);
                    })
                    .ok();
                cx.notify();
            }
        }
    }

    fn send(
        &mut self,
        message: JupyterMessage,
        _window: &mut Window,
        _cx: &mut ModelContext<Self>,
    ) -> anyhow::Result<()> {
        if let Kernel::RunningKernel(kernel) = &mut self.kernel {
            kernel.request_tx().try_send(message).ok();
        }

        anyhow::Ok(())
    }

    pub fn clear_outputs(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) {
        let blocks_to_remove: HashSet<CustomBlockId> =
            self.blocks.values().map(|block| block.block_id).collect();

        self.editor
            .update(cx, |editor, cx| {
                editor.remove_blocks(blocks_to_remove, None, window, cx);
            })
            .ok();

        self.blocks.clear();
    }

    pub fn execute(
        &mut self,
        code: String,
        anchor_range: Range<Anchor>,
        next_cell: Option<Anchor>,
        move_down: bool,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        let Some(editor) = self.editor.upgrade() else {
            return;
        };

        if code.is_empty() {
            return;
        }

        let execute_request = ExecuteRequest {
            code,
            ..ExecuteRequest::default()
        };

        let message: JupyterMessage = execute_request.into();

        let mut blocks_to_remove: HashSet<CustomBlockId> = HashSet::default();

        let buffer = editor.read(cx).buffer().read(cx).snapshot(cx);

        self.blocks.retain(|_key, block| {
            if anchor_range.overlaps(&block.code_range, &buffer) {
                blocks_to_remove.insert(block.block_id);
                false
            } else {
                true
            }
        });

        self.editor
            .update(cx, |editor, cx| {
                editor.remove_blocks(blocks_to_remove, None, window, cx);
            })
            .ok();

        let status = match &self.kernel {
            Kernel::Restarting => ExecutionStatus::Restarting,
            Kernel::RunningKernel(_) => ExecutionStatus::Queued,
            Kernel::StartingKernel(_) => ExecutionStatus::ConnectingToKernel,
            Kernel::ErroredLaunch(error) => ExecutionStatus::KernelErrored(error.clone()),
            Kernel::ShuttingDown => ExecutionStatus::ShuttingDown,
            Kernel::Shutdown => ExecutionStatus::Shutdown,
        };

        let parent_message_id = message.header.msg_id.clone();
        let session_view = cx.model().downgrade();
        let weak_editor = self.editor.clone();

        let on_close: CloseBlockFn = Arc::new(
            move |block_id: CustomBlockId, window: &mut Window, cx: &mut AppContext| {
                if let Some(session) = session_view.upgrade() {
                    session.update(cx, |session, cx| {
                        session.blocks.remove(&parent_message_id);
                        cx.notify();
                    });
                }

                if let Some(editor) = weak_editor.upgrade() {
                    editor.update(cx, |editor, cx| {
                        let mut block_ids = HashSet::default();
                        block_ids.insert(block_id);
                        editor.remove_blocks(block_ids, None, window, cx);
                    });
                }
            },
        );

        let Ok(editor_block) = EditorBlock::new(
            self.editor.clone(),
            anchor_range,
            status,
            on_close,
            window,
            cx,
        ) else {
            return;
        };

        let new_cursor_pos = if let Some(next_cursor) = next_cell {
            next_cursor
        } else {
            editor_block.invalidation_anchor
        };

        self.blocks
            .insert(message.header.msg_id.clone(), editor_block);

        match &self.kernel {
            Kernel::RunningKernel(_) => {
                self.send(message, window, cx).ok();
            }
            Kernel::StartingKernel(task) => {
                // Queue up the execution as a task to run after the kernel starts
                let task = task.clone();
                let message = message.clone();

                cx.spawn_in(window, |this, mut cx| async move {
                    task.await;
                    this.update_in(&mut cx, |session, window, cx| {
                        session.send(message, window, cx).ok();
                    })
                    .ok();
                })
                .detach();
            }
            _ => {}
        }

        if move_down {
            editor.update(cx, move |editor, cx| {
                editor.change_selections(
                    Some(Autoscroll::top_relative(8)),
                    window,
                    cx,
                    |selections| {
                        selections.select_ranges([new_cursor_pos..new_cursor_pos]);
                    },
                );
            });
        }
    }

    pub fn route(
        &mut self,
        message: &JupyterMessage,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        let parent_message_id = match message.parent_header.as_ref() {
            Some(header) => &header.msg_id,
            None => return,
        };

        match &message.content {
            JupyterMessageContent::Status(status) => {
                self.kernel.set_execution_state(&status.execution_state);

                telemetry::event!(
                    "Kernel Status Changed",
                    kernel_language = self.kernel_specification.language(),
                    kernel_status = KernelStatus::from(&self.kernel).to_string(),
                    repl_session_id = cx.entity_id().to_string(),
                );

                cx.notify();
            }
            JupyterMessageContent::KernelInfoReply(reply) => {
                self.kernel.set_kernel_info(reply);
                cx.notify();
            }
            JupyterMessageContent::UpdateDisplayData(update) => {
                let display_id = if let Some(display_id) = update.transient.display_id.clone() {
                    display_id
                } else {
                    return;
                };

                self.blocks.iter_mut().for_each(|(_, block)| {
                    block.execution_view.update(cx, |execution_view, cx| {
                        execution_view.update_display_data(&update.data, &display_id, window, cx);
                    });
                });
                return;
            }
            _ => {}
        }

        if let Some(block) = self.blocks.get_mut(parent_message_id) {
            block.handle_message(message, window, cx);
        }
    }

    pub fn interrupt(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) {
        match &mut self.kernel {
            Kernel::RunningKernel(_kernel) => {
                self.send(InterruptRequest {}.into(), window, cx).ok();
            }
            Kernel::StartingKernel(_task) => {
                // NOTE: If we switch to a literal queue instead of chaining on to the task, clear all queued executions
            }
            _ => {}
        }
    }

    pub fn kernel(&mut self, kernel: Kernel, window: &mut Window, cx: &mut ModelContext<Self>) {
        if let Kernel::Shutdown = kernel {
            cx.emit(SessionEvent::Shutdown(self.editor.clone()));
        }

        let kernel_status = KernelStatus::from(&kernel).to_string();
        let kernel_language = self.kernel_specification.language();

        telemetry::event!(
            "Kernel Status Changed",
            kernel_language,
            kernel_status,
            repl_session_id = cx.entity_id().to_string(),
        );

        self.kernel = kernel;
    }

    pub fn shutdown(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) {
        let kernel = std::mem::replace(&mut self.kernel, Kernel::ShuttingDown);

        match kernel {
            Kernel::RunningKernel(mut kernel) => {
                let mut request_tx = kernel.request_tx().clone();

                let forced = kernel.force_shutdown(window, cx);

                cx.spawn_in(window, |this, mut cx| async move {
                    let message: JupyterMessage = ShutdownRequest { restart: false }.into();
                    request_tx.try_send(message).ok();

                    forced.await.log_err();

                    // Give the kernel a bit of time to clean up
                    cx.background_executor().timer(Duration::from_secs(3)).await;

                    this.update_in(&mut cx, |session, window, cx| {
                        session.clear_outputs(window, cx);
                        session.kernel(Kernel::Shutdown, window, cx);
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            }
            _ => {
                self.kernel(Kernel::Shutdown, window, cx);
            }
        }
        cx.notify();
    }

    pub fn restart(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) {
        let kernel = std::mem::replace(&mut self.kernel, Kernel::Restarting);

        match kernel {
            Kernel::Restarting => {
                // Do nothing if already restarting
            }
            Kernel::RunningKernel(mut kernel) => {
                let mut request_tx = kernel.request_tx().clone();

                let forced = kernel.force_shutdown(window, cx);

                cx.spawn_in(window, |this, mut cx| async move {
                    // Send shutdown request with restart flag
                    log::debug!("restarting kernel");
                    let message: JupyterMessage = ShutdownRequest { restart: true }.into();
                    request_tx.try_send(message).ok();

                    // Wait for kernel to shutdown
                    cx.background_executor().timer(Duration::from_secs(1)).await;

                    // Force kill the kernel if it hasn't shut down
                    forced.await.log_err();

                    // Start a new kernel
                    this.update_in(&mut cx, |session, window, cx| {
                        // TODO: Differentiate between restart and restart+clear-outputs
                        session.clear_outputs(window, cx);
                        session.start_kernel(window, cx);
                    })
                    .ok();
                })
                .detach();
            }
            _ => {
                self.clear_outputs(window, cx);
                self.start_kernel(window, cx);
            }
        }
        cx.notify();
    }
}

pub enum SessionEvent {
    Shutdown(WeakModel<Editor>),
}

impl EventEmitter<SessionEvent> for Session {}

impl Render for Session {
    fn render(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) -> impl IntoElement {
        let (status_text, interrupt_button) = match &self.kernel {
            Kernel::RunningKernel(kernel) => (
                kernel
                    .kernel_info()
                    .as_ref()
                    .map(|info| info.language_info.name.clone()),
                Some(
                    Button::new("interrupt", "Interrupt")
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(move |session, _, window, cx| {
                            session.interrupt(window, cx);
                        })),
                ),
            ),
            Kernel::StartingKernel(_) => (Some("Starting".into()), None),
            Kernel::ErroredLaunch(err) => (Some(format!("Error: {err}")), None),
            Kernel::ShuttingDown => (Some("Shutting Down".into()), None),
            Kernel::Shutdown => (Some("Shutdown".into()), None),
            Kernel::Restarting => (Some("Restarting".into()), None),
        };

        KernelListItem::new(self.kernel_specification.clone())
            .status_color(match &self.kernel {
                Kernel::RunningKernel(kernel) => match kernel.execution_state() {
                    ExecutionState::Idle => Color::Success,
                    ExecutionState::Busy => Color::Modified,
                },
                Kernel::StartingKernel(_) => Color::Modified,
                Kernel::ErroredLaunch(_) => Color::Error,
                Kernel::ShuttingDown => Color::Modified,
                Kernel::Shutdown => Color::Disabled,
                Kernel::Restarting => Color::Modified,
            })
            .child(Label::new(self.kernel_specification.name()))
            .children(status_text.map(|status_text| Label::new(format!("({status_text})"))))
            .button(
                Button::new("shutdown", "Shutdown")
                    .style(ButtonStyle::Subtle)
                    .disabled(self.kernel.is_shutting_down())
                    .on_click(cx.listener(move |session, _, window, cx| {
                        session.shutdown(window, cx);
                    })),
            )
            .buttons(interrupt_button)
    }
}
