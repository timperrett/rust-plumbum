use std::fmt;

mod kleisli;
pub use kleisli::Kleisli;

pub mod extra;

mod pipe;
pub use pipe::*;

enum Void {}

/// Represents a conduit, i.e. a sequence of await/yield actions.
///
/// - `I` is the type of values the conduit consumes from upstream.
/// - `O` is the type of values the conduit passes downstream.
/// - `A` is the return type of the conduit.
pub enum ConduitM<'a, I, O, A> {
    /// The case `Pure(a)` means that the conduit contains requires further action and just returns the result `a`.
    Pure(Box<A>),
    /// The case `Await(k)` means that the conduit waits for a value of type `I`, and the remaining (suspended) program is given by the kleisli arrow `k`.
    Await(Kleisli<'a, Option<I>, I, O, A>),
    /// The case `Yield(o, k)` means that the conduit yields a value of type `O`, and the remaining (suspended) program is given by the kleisli arrow `k`.
    Yield(Box<O>, Kleisli<'a, (), I, O, A>)
}

/// Provides a stream of output values,
/// without consuming any input or producing a final result.
pub type Source<'a, O> = ConduitM<'a, (), O, ()>;

impl<'a, O> ConduitM<'a, (), O, ()> {

    /// Generalize a `Source` by universally quantifying the input type.
    pub fn to_producer<I>(self) -> ConduitM<'a, I, O, ()> where O: 'static {
        match self {
            ConduitM::Pure(x) => ConduitM::Pure(x),
            ConduitM::Await(k) => k.run(Some(())).to_producer(),
            ConduitM::Yield(o, k) => ConduitM::Yield(o, Kleisli::new().append(move |_| {
                k.run(()).to_producer()
            }))
        }
    }

    /// Pulls data from the source and pushes it into the sink.
    ///
    /// # Example
    ///
    /// ```rust
    /// use plumbum::{Source, Sink, consume, produce};
    ///
    /// let src = produce(42);
    ///
    /// let sink = consume().map(|x| 1 + x.unwrap_or(0));
    ///
    /// assert_eq!(src.connect(sink), 43);
    /// ```
    pub fn connect<B>(mut self, mut sink: Sink<'a, O, B>) -> B where O: 'static {
        loop {
            let (next_src, next_sink) = match sink {
                ConduitM::Pure(b_box) => {
                    return *b_box;
                },
                ConduitM::Await(k_sink) => {
                    match self {
                        ConduitM::Pure(x) => {
                            (ConduitM::Pure(x), k_sink.run(None))
                        },
                        ConduitM::Await(k_src) => {
                            (k_src.run(Some(())), ConduitM::Await(k_sink))
                        },
                        ConduitM::Yield(a_box, k_src) => {
                            (k_src.run(()), k_sink.run(Some(*a_box)))
                        }
                    }
                },
                ConduitM::Yield(_, _) => unreachable!()
            };
            self = next_src;
            sink = next_sink;
        }
    }

}

/// Consumes a stream of input values and produces a stream of output values,
/// without producing a final result.
pub type Conduit<'a, I, O> = ConduitM<'a, I, O, ()>;

impl<'a, I, O> ConduitM<'a, I, O, ()> {
    /// Combines two Conduits together into a new Conduit.
    ///
    /// # Example
    ///
    /// ```rust
    /// use plumbum::{Source, Sink, consume, produce};
    ///
    /// let src = produce(42);
    ///
    /// let conduit = consume().and_then(|x| produce(1 + x.unwrap_or(0)));
    ///
    /// let sink = consume().map(|x| 1 + x.unwrap_or(0));
    ///
    /// assert_eq!(src.fuse(conduit).connect(sink), 44);
    /// ```
    pub fn fuse<C, R>(self, other: ConduitM<'a, O, C, R>) -> ConduitM<'a, I, C, R>
        where I: 'static, O: 'static, C: 'static, R: 'a {
        match other {
            ConduitM::Pure(r) => ConduitM::Pure(r),
            ConduitM::Yield(c, k) => ConduitM::Yield(c, Kleisli::new().append(move |_| {
                self.fuse(k.run(()))
            })),
            ConduitM::Await(k_right) => match self {
                ConduitM::Pure(_) => ConduitM::fuse(().into(), k_right.run(None)),
                ConduitM::Yield(b, k_left) => k_left.run(()).fuse(k_right.run(Some(*b))),
                ConduitM::Await(k_left) => ConduitM::Await(Kleisli::new().append(move |a| {
                    k_left.run(a).fuse(ConduitM::Await(k_right))
                }))
            }
        }
    }
}

/// Consumes a stream of input values and produces a final result,
/// without producing any output.
pub type Sink<'a, I, A> = ConduitM<'a, I, Void, A>;

impl<'a, I, A> ConduitM<'a, I, Void, A> {
    /// Generalize a `Sink` by universally quantifying the output type.
    pub fn to_consumer<O>(self) -> ConduitM<'a, I, O, A> {
        unsafe { std::mem::transmute(self) }
    }
}

impl<'a, I, O, A> ConduitM<'a, I, O, A> {

    fn and_then_boxed<B, F>(self, js: F) -> ConduitM<'a, I, O, B>
        where F: 'a + FnOnce(Box<A>) -> ConduitM<'a, I, O, B> {
        match self {
            ConduitM::Pure(a) => js(a),
            ConduitM::Await(is) => ConduitM::Await(kleisli::append_boxed(is, js)),
            ConduitM::Yield(o, is) => ConduitM::Yield(o, kleisli::append_boxed(is, js))
        }
    }

    /// Appends a continuation to a conduit. Which means,
    /// given a function from `A` to `ConduitM<I, O, B>`,
    /// passes the return value of the conduit to the function,
    /// and returns the resulting program.
    pub fn and_then<B, F>(self, js: F) -> ConduitM<'a, I, O, B>
        where F: 'a + FnOnce(A) -> ConduitM<'a, I, O, B> {
        match self {
            ConduitM::Pure(a) => js(*a),
            ConduitM::Await(is) => ConduitM::Await(is.append(js)),
            ConduitM::Yield(o, is) => ConduitM::Yield(o, is.append(js))
        }
    }

    /// Modifies the return value of the conduit.
    /// Seen differently, it lifts a function from
    /// `A` to `B` into a function from `ConduitM<I, O, A>`
    /// to `ConduitM<I, O, B>`.
    pub fn map<B, F>(self, f: F) -> ConduitM<'a, I, O, B>
        where F: 'a + FnOnce(A) -> B {
        self.and_then(move |a| f(a).into())
    }

}

impl<'a, I, O, A: PartialEq> PartialEq for ConduitM<'a, I, O, A> {
    fn eq(&self, other: &ConduitM<'a, I, O, A>) -> bool {
        match (self, other) {
            (&ConduitM::Pure(ref a), &ConduitM::Pure(ref b)) => a == b,
            _ => false
        }
    }
}

impl<'a, I, O, A: fmt::Debug> fmt::Debug for ConduitM<'a, I, O, A> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &ConduitM::Pure(ref a) => write!(f, "Pure({:?})", a),
            &ConduitM::Await(_) => write!(f, "Await(..)"),
            &ConduitM::Yield(_, _) => write!(f, "Yield(..)")
        }
    }
}

impl<'a, I, O, A> From<A> for ConduitM<'a, I, O, A> {
    fn from(a: A) -> ConduitM<'a, I, O, A> {
        ConduitM::Pure(Box::new(a))
    }
}

/// Wait for a single input value from upstream.
///
/// If no data is available, returns `None`.
/// Once it returns `None`, subsequent calls will also return `None`.
pub fn consume<'a, I, O>() -> ConduitM<'a, I, O, Option<I>> {
    ConduitM::Await(Kleisli::new())
}

/// Send a value downstream to the next component to consume.
///
/// If the downstream component terminates, this call will never return control.
pub fn produce<'a, I, O>(o: O) -> ConduitM<'a, I, O, ()> {
    ConduitM::Yield(Box::new(o), Kleisli::new())
}
