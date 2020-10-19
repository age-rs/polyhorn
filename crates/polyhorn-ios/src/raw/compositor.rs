use dispatch::Queue;
use polyhorn_core::{Command, Composition};
use polyhorn_ui::layout::LayoutTree;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use super::{Environment, OpaqueContainer, Platform, QueueBound};

/// Concrete implementation of a compositor that is responsible for adding and
/// removing native views from the native view hierarchy based on the virtual
/// representation within Polyhorn.
#[derive(Clone)]
pub struct Compositor {
    buffer: Arc<QueueBound<Composition<Platform>>>,
    counter: Arc<AtomicUsize>,
    layout_tree: Arc<RwLock<LayoutTree>>,
}

impl Compositor {
    /// Returns a new compositor with the given shared layout tree.
    pub fn new(layout_tree: Arc<RwLock<LayoutTree>>) -> Compositor {
        Compositor {
            buffer: Arc::new(QueueBound::new(Queue::main(), || Default::default())),
            counter: Arc::new(AtomicUsize::default()),
            layout_tree,
        }
    }

    fn next_id(&mut self) -> ContainerID {
        let id = self.counter.fetch_add(1, Ordering::Relaxed);
        ContainerID(id)
    }

    pub(crate) fn track(&mut self, container: OpaqueContainer) -> ContainerID {
        let id = self.next_id();

        unsafe {
            self.buffer.with_adopt(container, move |state, container| {
                state.insert(id, container);
            });
        }

        id
    }
}

impl polyhorn_core::Compositor<Platform> for Compositor {
    fn buffer(&self) -> CommandBuffer {
        CommandBuffer {
            compositor: self.clone(),
            commands: vec![],
        }
    }
}

/// An opaque ID for containers that can be shared between threads.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ContainerID(usize);

/// Concrete implementation of a command buffer that can buffer commands before
/// committing them to the compositor.
pub struct CommandBuffer {
    compositor: Compositor,
    commands: Vec<Command<Platform>>,
}

impl polyhorn_core::CommandBuffer<Platform> for CommandBuffer {
    fn mount<F>(&mut self, parent_id: ContainerID, initializer: F) -> ContainerID
    where
        F: FnOnce(&mut OpaqueContainer, &mut Environment) -> OpaqueContainer + Send + 'static,
    {
        let id = self.compositor.next_id();
        self.commands
            .push(Command::Mount(id, parent_id, Box::new(initializer)));
        id
    }

    fn mutate<F>(&mut self, ids: &[ContainerID], mutator: F)
    where
        F: FnOnce(&mut [&mut OpaqueContainer], &mut Environment) + Send + 'static,
    {
        self.commands
            .push(Command::Mutate(ids.to_owned(), Box::new(mutator)));
    }

    fn unmount(&mut self, id: ContainerID) {
        self.commands.push(Command::Unmount(id));
    }

    fn layout(&mut self) {
        self.mutate(&[], |_, environment| {
            let mut layout_tree = environment.layout_tree().write().unwrap();
            layout_tree.recompute_roots();
        });
    }

    fn commit(mut self) {
        let commands = std::mem::take(&mut self.commands);

        let layout_tree = self.compositor.layout_tree.clone();

        self.compositor.buffer.with(move |state| {
            // Apply each command to this state.
            let mut environment = Environment::new(layout_tree.clone());
            for command in commands {
                state.process(&mut environment, command);
            }
        });
    }
}
