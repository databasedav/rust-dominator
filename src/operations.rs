use std::sync::{Arc, Mutex};
use std::mem::ManuallyDrop;
use stdweb::PromiseFuture;
use discard::{Discard, DiscardOnDrop};
use futures_signals::{cancelable_future, CancelableFutureHandle};
use futures_signals::signal::{Signal, SignalExt};
use futures_signals::signal_vec::{VecDiff, SignalVec, SignalVecExt, IntoSignalVec};
use dom_operations;
use dom::Dom;
use callbacks::Callbacks;
use std::iter::IntoIterator;
use stdweb::traits::INode;
use futures_core::Never;
use futures_core::future::Future;


// TODO this should probably be in stdweb
#[inline]
pub fn spawn_future<F>(future: F) -> DiscardOnDrop<CancelableFutureHandle>
    where F: Future<Item = (), Error = Never> + 'static {
    // TODO make this more efficient ?
    let (handle, future) = cancelable_future(future, |_| ());

    PromiseFuture::spawn_local(future);

    handle
}


#[inline]
pub fn for_each<A, B>(signal: A, mut callback: B) -> CancelableFutureHandle
    where A: Signal + 'static,
          B: FnMut(A::Item) + 'static {

    DiscardOnDrop::leak(spawn_future(signal.for_each(move |value| {
        callback(value);
        Ok(())
    })))
}


#[inline]
fn for_each_vec<A, B>(signal: A, mut callback: B) -> CancelableFutureHandle
    where A: SignalVec + 'static,
          B: FnMut(VecDiff<A::Item>) + 'static {

    DiscardOnDrop::leak(spawn_future(signal.for_each(move |value| {
        callback(value);
        Ok(())
    })))
}


/*
// TODO inline this ?
pub fn insert_children_signal<A, B, C>(element: &A, callbacks: &mut Callbacks, signal: C)
    where A: INode + Clone + 'static,
          B: IntoIterator<Item = Dom>,
          C: Signal<Item = B> + 'static {

    let element = element.clone();

    let mut old_children: Vec<Dom> = vec![];

    let handle = for_each(signal, move |value| {
        dom_operations::remove_all_children(&element);

        old_children = value.into_iter().map(|mut dom| {
            element.append_child(&dom.element);

            // TODO don't trigger this if the parent isn't inserted into the DOM
            dom.callbacks.trigger_after_insert();

            dom
        }).collect();
    });

    // TODO verify that this will drop `old_children`
    callbacks.after_remove(handle);
}*/

#[inline]
pub fn insert_children_iter<'a, A: INode, B: IntoIterator<Item = &'a mut Dom>>(element: &A, callbacks: &mut Callbacks, value: B) {
    for dom in value.into_iter() {
        callbacks.after_insert.append(&mut dom.callbacks.after_insert);
        callbacks.after_remove.append(&mut dom.callbacks.after_remove);

        element.append_child(&dom.element);
    }
}


// TODO move this into the discard crate
// TODO verify that this is correct and doesn't leak memory or cause memory safety
pub struct ValueDiscard<A>(ManuallyDrop<A>);

impl<A> ValueDiscard<A> {
    #[inline]
    pub fn new(value: A) -> Self {
        ValueDiscard(ManuallyDrop::new(value))
    }
}

impl<A> Discard for ValueDiscard<A> {
    #[inline]
    fn discard(self) {
        // TODO verify that this works
        ManuallyDrop::into_inner(self.0);
    }
}


// TODO move this into the discard crate
// TODO replace this with an impl for FnOnce() ?
pub struct FnDiscard<A>(A);

impl<A> FnDiscard<A> where A: FnOnce() {
    #[inline]
    pub fn new(f: A) -> Self {
        FnDiscard(f)
    }
}

impl<A> Discard for FnDiscard<A> where A: FnOnce() {
    #[inline]
    fn discard(self) {
        self.0();
    }
}


pub fn insert_children_signal_vec<A, B>(element: &A, callbacks: &mut Callbacks, signal: B)
    where A: INode + Clone + 'static,
          B: IntoSignalVec<Item = Dom>,
          B::SignalVec: 'static {

    let element = element.clone();

    // TODO does this create a new struct type every time ?
    struct State {
        is_inserted: bool,
        children: Vec<Dom>,
    }

    // TODO use two separate Arcs ?
    let state = Arc::new(Mutex::new(State {
        is_inserted: false,
        children: vec![],
    }));

    {
        let state = state.clone();

        callbacks.after_insert(move |_| {
            let mut state = state.lock().unwrap();

            if !state.is_inserted {
                state.is_inserted = true;

                for dom in state.children.iter_mut() {
                    dom.callbacks.trigger_after_insert();
                }
            }
        });
    }

    // TODO verify that this will drop `children`
    callbacks.after_remove(for_each_vec(signal.into_signal_vec(), move |change| {
        let mut state = state.lock().unwrap();

        match change {
            VecDiff::Replace { values } => {
                // TODO is this correct ?
                if state.children.len() > 0 {
                    dom_operations::remove_all_children(&element);

                    for dom in state.children.drain(..) {
                        dom.callbacks.discard();
                    }
                }

                state.children = values;

                let is_inserted = state.is_inserted;

                // TODO use document fragment ?
                for dom in state.children.iter_mut() {
                    dom.callbacks.leak();

                    element.append_child(&dom.element);

                    if is_inserted {
                        dom.callbacks.trigger_after_insert();
                    }
                }
            },

            VecDiff::InsertAt { index, mut value } => {
                // TODO better usize -> u32 conversion
                dom_operations::insert_at(&element, index as u32, &value.element);

                value.callbacks.leak();

                if state.is_inserted {
                    value.callbacks.trigger_after_insert();
                }

                // TODO figure out a way to move this to the top
                state.children.insert(index, value);
            },

            VecDiff::Push { mut value } => {
                element.append_child(&value.element);

                value.callbacks.leak();

                if state.is_inserted {
                    value.callbacks.trigger_after_insert();
                }

                // TODO figure out a way to move this to the top
                state.children.push(value);
            },

            VecDiff::UpdateAt { index, mut value } => {
                // TODO better usize -> u32 conversion
                dom_operations::update_at(&element, index as u32, &value.element);

                value.callbacks.leak();

                if state.is_inserted {
                    value.callbacks.trigger_after_insert();
                }

                // TODO figure out a way to move this to the top
                // TODO test this
                ::std::mem::swap(&mut state.children[index], &mut value);

                value.callbacks.discard();
            },

            VecDiff::Move { old_index, new_index } => {
                let value = state.children.remove(old_index);

                state.children.insert(new_index, value);

                // TODO better usize -> u32 conversion
                dom_operations::move_from_to(&element, old_index as u32, new_index as u32);
            },

            VecDiff::RemoveAt { index } => {
                // TODO better usize -> u32 conversion
                dom_operations::remove_at(&element, index as u32);

                state.children.remove(index).callbacks.discard();
            },

            VecDiff::Pop {} => {
                let index = state.children.len() - 1;

                // TODO create remove_last_child function ?
                // TODO better usize -> u32 conversion
                dom_operations::remove_at(&element, index as u32);

                state.children.pop().unwrap().callbacks.discard();
            },

            VecDiff::Clear {} => {
                // TODO is this correct ?
                // TODO is this needed, or is it guaranteed by VecDiff ?
                if state.children.len() > 0 {
                    dom_operations::remove_all_children(&element);

                    for dom in state.children.drain(..) {
                        dom.callbacks.discard();
                    }
                }
            },
        }
    }));
}
