use super::element::{ElementBuiltin, ElementComponent, ElementContext, ElementFragment};
use super::{
    Bus, CommandBuffer, Component, Compositor, Disposable, Element, Instance, Link, Manager,
    Platform,
};
use std::cell::RefCell;
use std::ops::DerefMut;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

pub struct Render<P>
where
    P: Platform + ?Sized,
{
    renderer: Rc<Renderer<P>>,
    buffer: P::CommandBuffer,
}

impl<P> Render<P>
where
    P: Platform + ?Sized,
{
    fn new(renderer: Rc<Renderer<P>>) -> Render<P> {
        let buffer = renderer
            .compositor
            .try_borrow_mut()
            .expect("Couldn't acquire new command buffer from busy compositor.")
            .buffer();

        Render { renderer, buffer }
    }

    fn rerender_builtin(&mut self, instance: &Rc<Instance<P>>, element: ElementBuiltin<P>) {
        self.rerender_edges(instance, vec![*element.children]);
    }

    fn rerender_component(&mut self, instance: &Rc<Instance<P>>, element: ElementComponent<P>) {
        let (edges, effects) = {
            let mut memory = instance.memory_mut();
            let compositor = self
                .renderer
                .compositor
                .try_borrow()
                .expect("Couldn't borrow compositor.");
            let bus = self
                .renderer
                .bus
                .try_borrow()
                .expect("Couldn't borrow bus.");
            let link = Link::new(Rc::downgrade(instance));
            let mut manager = Manager::new(
                &*compositor,
                &*bus,
                &mut memory,
                instance.context(),
                *element.children,
                link,
            );
            (
                vec![element.component.render(&mut manager)],
                manager.into_effects(),
            )
        };

        self.rerender_edges(instance, edges);

        // Finally, we apply the effects and we're done!
        for effect in effects {
            effect(&mut self.buffer);
        }
    }

    fn rerender_context(&mut self, instance: &Rc<Instance<P>>, element: ElementContext<P>) {
        instance.context().insert_raw(element.value);

        self.rerender_edges(instance, vec![*element.children])
    }

    fn rerender_fragment(&mut self, instance: &Rc<Instance<P>>, element: ElementFragment<P>) {
        self.rerender_edges(instance, element.elements)
    }

    fn rerender_edges(&mut self, instance: &Rc<Instance<P>>, edges: Vec<Element<P>>) {
        // let missing_edges = memory.keep_edges(edges.iter().map(|element| element.key()));
        let mut memory = instance.memory_mut();
        let memory = memory.deref_mut();

        // Re-rendering looks a bit like mark and sweep. We start by collecting
        // the set of keys of edges.
        let mut keys = memory.keys();

        for element in edges {
            let key = element.key();

            keys.remove(key);

            if let Some(existing) = memory.edge(key) {
                // The edge already exists. We replace its element and issue a
                // re-render.
                existing.memory_mut().deref_mut().update(element);
                self.rerender(existing)
            } else {
                // The edge does not yet exist. We issue a fresh render and store
                // the resulting instance in the memory of this instance.
                let key = key.clone();
                let instance = self.render(
                    Some(instance.clone()),
                    element,
                    instance.container().clone(),
                );
                memory.add_edge(key, instance);
            }
        }

        // Finally, we unmount all instances that correspond to edges that are
        // no longer present.
        for key in keys {
            if let Some(instance) = memory.remove_edge(&key) {
                self.unmount(&instance);
            }
        }
    }

    fn unmount(&mut self, instance: &Rc<Instance<P>>) {
        for edge in instance.memory_mut().edges() {
            self.unmount(&edge);
        }

        match instance.memory_mut().deref_mut().element() {
            Element::Builtin(_) => {
                self.buffer.unmount(instance.container());
            }
            _ => {}
        }
    }

    /// This function is called when re-rendering an existing instance.
    pub fn rerender(&mut self, instance: &Rc<Instance<P>>) {
        let element = instance.memory_mut().element().clone();

        match element {
            Element::Builtin(element) => self.rerender_builtin(instance, element),
            Element::Component(element) => self.rerender_component(instance, element),
            Element::Context(element) => self.rerender_context(instance, element),
            Element::Fragment(element) => self.rerender_fragment(instance, element),
            Element::String(_text) => unimplemented!("Can't render string element directly."),
        }
    }

    /// This function is called when rendering an element into a container for
    /// the first time.
    pub fn render(
        &mut self,
        parent: Option<Rc<Instance<P>>>,
        element: Element<P>,
        in_container: P::ContainerID,
    ) -> Rc<Instance<P>> {
        // We start by figuring out if we need to create a new container for this
        // element or not.
        let container = match &element {
            Element::Builtin(element) => {
                let builtin = element.builtin.clone();
                let environment = self.renderer.environment.clone();
                let container = self.buffer.mount(in_container, move || {
                    builtin.instantiate(&mut *environment.lock().unwrap())
                });

                if let Some(reference) = &element.reference {
                    reference.replace(container);
                }

                container
            }
            _ => in_container,
        };

        let renderer = self.renderer.clone();

        // Then, we create an instance for this element.
        let instance = Rc::new(Instance::new(renderer, parent, element, container));

        // Finally, we pretend that this is simply a re-render.
        self.rerender(&instance);

        instance
    }
}

pub struct Renderer<P>
where
    P: Platform + ?Sized,
{
    compositor: RefCell<P::Compositor>,
    bus: RefCell<P::Bus>,
    environment: Arc<Mutex<P::Environment>>,
}

impl<P> Renderer<P>
where
    P: Platform + ?Sized,
{
    /// This function returns a new reference counted renderer with the given
    /// compositor.
    pub fn new(
        compositor: P::Compositor,
        bus: P::Bus,
        environment: P::Environment,
    ) -> Rc<Renderer<P>> {
        Rc::new(Renderer {
            compositor: RefCell::new(compositor),
            bus: RefCell::new(bus),
            environment: Arc::new(Mutex::new(environment)),
        })
    }

    pub fn rerender(self: &Rc<Self>, instance: &Rc<Instance<P>>) {
        let renderer = self.clone();
        let instance = instance.clone();

        self.bus.borrow().queue_retain(async move {
            let mut render = Render::new(renderer);
            render.rerender(&instance);
            render.buffer.commit();
        });
    }

    pub fn render(
        self: &Rc<Self>,
        element: Element<P>,
        container: P::ContainerID,
    ) -> Rc<Instance<P>> {
        let mut render = Render::new(self.clone());
        let instance = render.render(None, element, container);
        render.buffer.commit();

        instance
    }
}

/// This is the entry point of Polyhorn. This function renders an element into
/// the given container. The returned instance must be retained. Once the
/// returned is dropped, all UI will be unmounted.
///
/// Example
/// -------
/// ```rust
/// polyhorn::render(poly!(<Window />), Container::root())
/// ```
pub fn render<F, P>(element: F, container: P::Container) -> Disposable
where
    F: FnOnce() -> Element<P> + Send + 'static,
    P: Platform + ?Sized,
{
    P::with_compositor(
        container,
        move |container_id, compositor, bus, environment| {
            // We've now switched to the render thread.
            let renderer = Renderer::new(compositor, bus, environment);
            Disposable::new(renderer.render(element(), container_id))
        },
    )
}
