//! The `layers.library` conformance behaviour tests.
//!
//! A layer is a rectangle of pixels with a place in a z-order. The tests
//! cover the creation and destruction of a layer, and the fact that a layer
//! handle stops naming anything once the layer is destroyed.

use vela_kernel_layers::errors as layers_errors;
use vela_kernel_syscall::numbers::{self, SyscallArgs};

use vela_test_api_conformance::harness::{self, ok};

/// `CreateLayer` and `DestroyLayer`: the handle names a layer, and stops
/// naming one once the layer is destroyed.
#[test]
fn layers_create_and_destroy() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let layer = ok(
        harness::call(
            &k,
            t,
            numbers::LAYERS_CREATE,
            SyscallArgs::new(0, 0, 64, 64, 0, 0),
        ),
        "CreateLayer",
    );
    assert_ne!(layer.value, 0, "a layer handle must not be zero");

    // The layer exists.
    assert!(
        k.intuition()
            .layers()
            .exists(vela_kernel_layers::LayerId(layer.value as u32)),
        "the layer must exist after it was created",
    );

    ok(
        harness::call(
            &k,
            t,
            numbers::LAYERS_DESTROY,
            SyscallArgs::new(layer.value, 0, 0, 0, 0, 0),
        ),
        "DestroyLayer",
    );

    // And now it does not.
    assert!(
        !k.intuition()
            .layers()
            .exists(vela_kernel_layers::LayerId(layer.value as u32)),
        "the layer must be gone after it was destroyed",
    );

    // Destroying it again reports `NO_OBJECT` rather than succeeding.
    let twice = harness::call(
        &k,
        t,
        numbers::LAYERS_DESTROY,
        SyscallArgs::new(layer.value, 0, 0, 0, 0, 0),
    );
    assert!(twice.is_err(), "destroying a dead layer must fail");
    assert_eq!(twice.error, layers_errors::NO_OBJECT);
}

/// `BringToFront` puts a layer at the top of the stack.
///
/// The z-order is what decides which layer a program sees, so a call that
/// claims to reorder the stack must actually do it.
#[test]
fn layers_front_moves_a_layer_to_the_top() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let back = ok(
        harness::call(
            &k,
            t,
            numbers::LAYERS_CREATE,
            SyscallArgs::new(0, 0, 32, 32, 0, 0),
        ),
        "CreateLayer (back)",
    )
    .value;
    let front = ok(
        harness::call(
            &k,
            t,
            numbers::LAYERS_CREATE,
            SyscallArgs::new(0, 0, 32, 32, 0, 0),
        ),
        "CreateLayer (front)",
    )
    .value;

    ok(
        harness::call(
            &k,
            t,
            numbers::LAYERS_FRONT,
            SyscallArgs::new(back, 0, 0, 0, 0, 0),
        ),
        "BringToFront",
    );

    let order = k.intuition().layers().order();
    let back_index = order
        .iter()
        .position(|id| id.0 == back as u32)
        .expect("the layer must be in the stack");
    let front_index = order
        .iter()
        .position(|id| id.0 == front as u32)
        .expect("the layer must be in the stack");
    assert!(
        back_index > front_index,
        "the layer brought to the front must be later in the order",
    );
}
