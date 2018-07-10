//! An unbounded set of futures.

use crate::task::AtomicWaker;
use futures_core::future::Future;
use futures_core::stream::Stream;
use futures_core::task::{self, Poll};
use std::cell::UnsafeCell;
use std::fmt::{self, Debug};
use std::iter::FromIterator;
use std::marker::{PhantomData, Unpin};
use std::mem::{self, PinMut};
use std::ptr;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicPtr, AtomicBool};
use std::sync::{Arc, Weak};
use std::usize;

mod abort;

mod iter;
use self::iter::{IterMut, IterPinMut};

mod node;
use self::node::Node;

mod ready_to_run_queue;
use self::ready_to_run_queue::{ReadyToRunQueue, Dequeue};

/// A set of `Future`s which may complete in any order.
///
/// This structure is optimized to manage a large number of futures.
/// Futures managed by `FuturesUnordered` will only be polled when they
/// generate notifications. This reduces the required amount of work needed to
/// poll large numbers of futures.
///
/// `FuturesUnordered` can be filled by `collect`ing an iterator of `Future`s
/// into a `FuturesUnordered`, or by `push`ing `Future`s onto an existing
/// `FuturesUnordered`. When new `Future`s are added, `poll_next` must be
/// called in order to begin receiving wakeups for new `Future`s.
///
/// Note that you can create a ready-made `FuturesUnordered` via the
/// `futures_unordered` function in the `stream` module, or you can start with
/// an empty set with the `FuturesUnordered::new` constructor.
#[must_use = "streams do nothing unless polled"]
pub struct FuturesUnordered<F> {
    ready_to_run_queue: Arc<ReadyToRunQueue<F>>,
    len: usize,
    head_all: *const Node<F>,
}

unsafe impl<T: Send> Send for FuturesUnordered<T> {}
unsafe impl<T: Sync> Sync for FuturesUnordered<T> {}
impl<T> Unpin for FuturesUnordered<T> {}

// FuturesUnordered is implemented using two linked lists. One which links all
// futures managed by a `FuturesUnordered` and one that tracks futures that have
// been scheduled for polling. The first linked list is not thread safe and is
// only accessed by the thread that owns the `FuturesUnordered` value. The
// second linked list is an implementation of the intrusive MPSC queue algorithm
// described by 1024cores.net.
//
// When a future is submitted to the set a node is allocated and inserted in
// both linked lists. The next call to `poll_next` will (eventually) see this
// node and call `poll` on the future.
//
// Before a managed future is polled, the current task's `Waker` is replaced
// with one that is aware of the specific future being run. This ensures that
// task notifications generated by that specific future are visible to
// `FuturesUnordered`. When a notification is received, the node is scheduled
// for polling by being inserted into the concurrent linked list.
//
// Each node is wrapped in an `Arc` and thereby atomically reference counted.
// Also, each node contains an `AtomicBool` which acts as a flag that indicates
// whether the node is currently inserted in the atomic queue. When the future
// is notified, it will only insert itself into the linked list if it isn't
// currently inserted.

impl<T: Future> FuturesUnordered<T> {
    /// Constructs a new, empty `FuturesUnordered`
    ///
    /// The returned `FuturesUnordered` does not contain any futures.
    /// In this state, `FuturesUnordered::poll_next` will return
    /// `Poll::Ready(None)`.
    pub fn new() -> FuturesUnordered<T> {
        let stub = Arc::new(Node {
            future: UnsafeCell::new(None),
            next_all: UnsafeCell::new(ptr::null()),
            prev_all: UnsafeCell::new(ptr::null()),
            next_ready_to_run: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(true),
            ready_to_run_queue: Weak::new(),
        });
        let stub_ptr = &*stub as *const Node<T>;
        let ready_to_run_queue = Arc::new(ReadyToRunQueue {
            parent: AtomicWaker::new(),
            head: AtomicPtr::new(stub_ptr as *mut _),
            tail: UnsafeCell::new(stub_ptr),
            stub,
        });

        FuturesUnordered {
            len: 0,
            head_all: ptr::null_mut(),
            ready_to_run_queue,
        }
    }
}

impl<T: Future> Default for FuturesUnordered<T> {
    fn default() -> FuturesUnordered<T> {
        FuturesUnordered::new()
    }
}

impl<T> FuturesUnordered<T> {
    /// Returns the number of futures contained in the set.
    ///
    /// This represents the total number of in-flight futures.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the set contains no futures
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push a future into the set.
    ///
    /// This function submits the given future to the set for managing. This
    /// function will not call `poll` on the submitted future. The caller must
    /// ensure that `FuturesUnordered::poll_next` is called in order to receive
    /// task notifications.
    pub fn push(&mut self, future: T) {
        let node = Arc::new(Node {
            future: UnsafeCell::new(Some(future)),
            next_all: UnsafeCell::new(ptr::null_mut()),
            prev_all: UnsafeCell::new(ptr::null_mut()),
            next_ready_to_run: AtomicPtr::new(ptr::null_mut()),
            queued: AtomicBool::new(true),
            ready_to_run_queue: Arc::downgrade(&self.ready_to_run_queue),
        });

        // Right now our node has a strong reference count of 1. We transfer
        // ownership of this reference count to our internal linked list
        // and we'll reclaim ownership through the `unlink` function below.
        let ptr = self.link(node);

        // We'll need to get the future "into the system" to start tracking it,
        // e.g. getting its unpark notifications going to us tracking which
        // futures are ready. To do that we unconditionally enqueue it for
        // polling here.
        self.ready_to_run_queue.enqueue(ptr);
    }

    /// Returns an iterator that allows modifying each future in the set.
    pub fn iter_mut(&mut self) -> IterMut<T> where T: Unpin {
        IterMut(PinMut::new(self).iter_pin_mut())
    }

    /// Returns an iterator that allows modifying each future in the set.
    pub fn iter_pin_mut<'a>(self: PinMut<'a, Self>) -> IterPinMut<'a, T> {
        IterPinMut {
            node: self.head_all,
            len: self.len,
            _marker: PhantomData
        }
    }

    /// Releases the node. It destorys the future inside and either drops
    /// the `Arc<Node>` or transfers ownership to the ready to run queue.
    /// The node this method is called on must have been unlinked before.
    fn release_node(&mut self, node: Arc<Node<T>>) {
        // `release_node` must only be called on unlinked nodes
        unsafe {
            debug_assert!((*node.next_all.get()).is_null());
            debug_assert!((*node.prev_all.get()).is_null());
        }

        // The future is done, try to reset the queued flag. This will prevent
        // `notify` from doing any work in the future
        let prev = node.queued.swap(true, SeqCst);

        // Drop the future, even if it hasn't finished yet. This is safe
        // because we're dropping the future on the thread that owns
        // `FuturesUnordered`, which correctly tracks T's lifetimes and such.
        unsafe {
            drop((*node.future.get()).take());
        }

        // If the queued flag was previously set, then it means that this node
        // is still in our internal ready to run queue. We then transfer
        // ownership of our reference count to the ready to run queue, and it'll
        // come along and free it later, noticing that the future is `None`.
        //
        // If, however, the queued flag was *not* set then we're safe to
        // release our reference count on the internal node. The queued flag
        // was set above so all future `enqueue` operations will not actually
        // enqueue the node, so our node will never see the ready to run queue
        // again. The node itself will be deallocated once all reference counts
        // have been dropped by the various owning tasks elsewhere.
        if prev {
            mem::forget(node);
        }
    }

    /// Insert a new node into the internal linked list.
    fn link(&mut self, node: Arc<Node<T>>) -> *const Node<T> {
        let ptr = Arc::into_raw(node);
        unsafe {
            *(*ptr).next_all.get() = self.head_all;
            if !self.head_all.is_null() {
                *(*self.head_all).prev_all.get() = ptr;
            }
        }

        self.head_all = ptr;
        self.len += 1;
        ptr
    }

    /// Remove the node from the linked list tracking all nodes currently
    /// managed by `FuturesUnordered`.
    /// This function is unsafe because it has be guaranteed that `node` is a
    /// valid pointer.
    unsafe fn unlink(&mut self, node: *const Node<T>) -> Arc<Node<T>> {
        let node = Arc::from_raw(node);
        let next = *node.next_all.get();
        let prev = *node.prev_all.get();
        *node.next_all.get() = ptr::null_mut();
        *node.prev_all.get() = ptr::null_mut();

        if !next.is_null() {
            *(*next).prev_all.get() = prev;
        }

        if !prev.is_null() {
            *(*prev).next_all.get() = next;
        } else {
            self.head_all = next;
        }
        self.len -= 1;
        node
    }
}

impl<T: Future> Stream for FuturesUnordered<T> {
    type Item = T::Output;

    fn poll_next(mut self: PinMut<Self>, cx: &mut task::Context)
        -> Poll<Option<Self::Item>>
    {
        // Ensure `parent` is correctly set.
        self.ready_to_run_queue.parent.register(cx.waker());

        loop {
            // Safety: &mut self guarantees the mutual exclusion `dequeue`
            // expects
            let node = match unsafe { self.ready_to_run_queue.dequeue() } {
                Dequeue::Empty => {
                    if self.is_empty() {
                        return Poll::Ready(None);
                    } else {
                        return Poll::Pending;
                    }
                }
                Dequeue::Inconsistent => {
                    // At this point, it may be worth yielding the thread &
                    // spinning a few times... but for now, just yield using the
                    // task system.
                    cx.local_waker().wake();
                    return Poll::Pending;
                }
                Dequeue::Data(node) => node,
            };

            debug_assert!(node != self.ready_to_run_queue.stub());

            // Safety:
            // - Node is a valid pointer.
            // - We are the only thread that accesses the `UnsafeCell` that
            //   contains the future
            let future = match unsafe { &mut *(*node).future.get() } {
                Some(future) => future,

                // If the future has already gone away then we're just
                // cleaning out this node. See the comment in
                // `release_node` for more information, but we're basically
                // just taking ownership of our reference count here.
                None => {
                    // This case only happens when `release_node` was called
                    // for this node before and couldn't drop the node
                    // because it was already enqueued in the ready to run
                    // queue.

                    // Safety: `node` is a valid pointer
                    let node = unsafe { Arc::from_raw(node) };

                    // Double check that the call to `release_node` really
                    // happened. Calling it required the node to be unlinked.
                    unsafe {
                        debug_assert!((*node.next_all.get()).is_null());
                        debug_assert!((*node.prev_all.get()).is_null());
                    }
                    continue
                }
            };

            // Safety: `node` is a valid pointer
            let node = unsafe { self.unlink(node) };

            // Unset queued flag... this must be done before
            // polling. This ensures that the future gets
            // rescheduled if it is notified **during** a call
            // to `poll`.
            let prev = node.queued.swap(false, SeqCst);
            assert!(prev);

            let local_waker = node.local_waker();

            // We're going to need to be very careful if the `poll`
            // function below panics. We need to (a) not leak memory and
            // (b) ensure that we still don't have any use-after-frees. To
            // manage this we do a few things:
            //
            // * A "bomb" is created which if dropped abnormally will call
            //   `release_node`. That way we'll be sure the memory management
            //   of the `node` is managed correctly. In particular
            //   `release_node` will drop the future. This ensures that it is
            //   dropped on this thread and not accidentally on a different
            //   thread (bad).
            // * We unlink the node from our internal queue to preemptively
            //   assume it'll panic, in which case we'll want to discard it
            //   regardless.
            struct Bomb<'a, T: 'a> {
                queue: &'a mut FuturesUnordered<T>,
                node: Option<Arc<Node<T>>>,
            }

            impl<'a, T> Drop for Bomb<'a, T> {
                fn drop(&mut self) {
                    if let Some(node) = self.node.take() {
                        self.queue.release_node(node);
                    }
                }
            }

            let mut bomb = Bomb {
                node: Some(node),
                queue: &mut *self,
            };

            // Poll the underlying future with the appropriate waker
            // implementation. This is where a large bit of the unsafety
            // starts to stem from internally. The waker is basically just
            // our `Arc<Node<T>>` and can schedule the future for polling by
            // enqueuing its node in the ready to run queue.
            //
            // Critically though `Node<T>` won't actually access `T`, the
            // future, while it's floating around inside of `Task`
            // instances. These structs will basically just use `T` to size
            // the internal allocation, appropriately accessing fields and
            // deallocating the node if need be.

            // Safety: We won't move the future ever again
            let future = unsafe { PinMut::new_unchecked(future) };

            let mut cx = cx.with_waker(&local_waker);
            let res = future.poll(&mut cx);

            let ret = match res {
                Poll::Pending => {
                    let node = bomb.node.take().unwrap();
                    bomb.queue.link(node);
                    continue
                }
                Poll::Ready(result) => Poll::Ready(Some(result)),
            };
            return ret
        }
    }
}

impl<T> Debug for FuturesUnordered<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "FuturesUnordered {{ ... }}")
    }
}

impl<T> Drop for FuturesUnordered<T> {
    fn drop(&mut self) {
        // When a `FuturesUnordered` is dropped we want to drop all futures
        // associated with it. At the same time though there may be tons of
        // `Task` handles flying around which contain `Node<T>` references
        // inside them. We'll let those naturally get deallocated when the
        // `Task` itself goes out of scope or gets notified.
        unsafe {
            while !self.head_all.is_null() {
                let head = self.head_all;
                let node = self.unlink(head);
                self.release_node(node);
            }
        }

        // Note that at this point we could still have a bunch of nodes in the
        // ready to run queue. None of those nodes, however, have futures
        // associated with them so they're safe to destroy on any thread. At
        // this point the `FuturesUnordered` struct, the owner of the one strong
        // reference to the ready to run queue will drop the strong reference.
        // At that point whichever thread releases the strong refcount last (be
        // it this thread or some other thread as part of an `upgrade`) will
        // clear out the ready to run queue and free all remaining nodes.
        //
        // While that freeing operation isn't guaranteed to happen here, it's
        // guaranteed to happen "promptly" as no more "blocking work" will
        // happen while there's a strong refcount held.
    }
}

impl<F: Future> FromIterator<F> for FuturesUnordered<F> {
    fn from_iter<T>(iter: T) -> Self
    where
        T: IntoIterator<Item = F>,
    {
        let acc = FuturesUnordered::new();
        iter.into_iter().fold(acc, |mut acc, item| { acc.push(item); acc })
    }
}

/// Converts a list of futures into a `Stream` of results from the futures.
///
/// This function will take an list of futures (e.g. a vector, an iterator,
/// etc), and return a stream. The stream will yield items as they become
/// available on the futures internally, in the order that they become
/// available. This function is similar to `buffer_unordered` in that it may
/// return items in a different order than in the list specified.
///
/// Note that the returned set can also be used to dynamically push more
/// futures into the set as they become available.
pub fn futures_unordered<I>(futures: I) -> FuturesUnordered<I::Item>
where
    I: IntoIterator,
    I::Item: Future,
{
    futures.into_iter().collect()
}
