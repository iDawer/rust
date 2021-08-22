// check-pass

#![feature(generic_associated_types)]

trait X {}
trait Y<YA: X> {}

trait Foo {
    type Bar<A: X>: Y<A>;
}

impl Foo for () {
    type Bar<A: X> = ();
}

impl<A: X> Y<A> for () {}

fn main() {}
