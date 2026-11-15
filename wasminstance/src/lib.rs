mod host;
mod messages;
mod raft;

// The exports the host calls, generated from wit/node.wit. Native builds
// leave this out and only run the unit tests.
#[cfg(target_arch = "wasm32")]
mod component {
    use std::cell::RefCell;

    use crate::host;
    use crate::raft::InstanceState;

    wit_bindgen::generate!({
        world: "node",
        path: "../wit",
    });

    thread_local! {
        // Wasm is single-threaded and the host never calls back into us
        // while we're running, so there's only ever one borrow at a time.
        static INSTANCE: RefCell<Option<InstanceState>> = const { RefCell::new(None) };
    }

    fn with_instance(f: impl FnOnce(&mut InstanceState)) {
        INSTANCE.with(|instance| {
            if let Some(instance) = instance.borrow_mut().as_mut() {
                f(instance);
            }
        });
    }

    struct Node;

    impl Guest for Node {
        fn start(id: i32, cluster_size: i32) {
            let mut instance = InstanceState::new(id, cluster_size);
            instance.start();
            INSTANCE.with(|slot| *slot.borrow_mut() = Some(instance));
        }

        fn receive(sender: i32, data: Vec<u8>) {
            with_instance(|instance| instance.receive(sender, &data));
        }

        fn tick(now_ms: i32) {
            with_instance(|instance| instance.tick(now_ms));
        }

        fn restart(now_ms: i32) {
            with_instance(|instance| instance.restart(now_ms));
        }

        fn status() {
            with_instance(|instance| host::log(&instance.status()));
        }
    }

    export!(Node);
}
