use super::utils::GTK_EVENT_HANDLER;
use super::utils::GTK_MUTEX;

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use std::task::{Context, Poll, Waker};

use super::utils::gtk_init_check;
use super::AsGtkDialog;

struct FutureState<R, D> {
    waker: Option<Waker>,
    data: Option<R>,
    dialog: Option<D>,
}

unsafe impl<R, D> Send for FutureState<R, D> {}

pub(super) struct GtkDialogFuture<R, D> {
    state: Arc<Mutex<FutureState<R, D>>>,
}

unsafe impl<R, D> Send for GtkDialogFuture<R, D> {}

impl<R: Default + 'static, D: AsGtkDialog + 'static> GtkDialogFuture<R, D> {
    pub fn new<B, F>(build: B, cb: F) -> Self
    where
        B: FnOnce() -> D + Send + 'static,
        F: Fn(&mut D, i32) -> R + Send + 'static,
    {
        let state = Arc::new(Mutex::new(FutureState {
            waker: None,
            data: None,
            dialog: None,
        }));

        {
            let state = state.clone();
            std::thread::spawn(move || {
                let request = Rc::new(RefCell::new(None));

                let callback = {
                    let state = state.clone();
                    let request = request.clone();

                    // Callbacks are called by GTK_EVENT_HANDLER so the GTK_MUTEX is allready locked, no need to worry about that here
                    move |res_id| {
                        let mut state = state.lock().unwrap();

                        if let Some(mut dialog) = state.dialog.take() {
                            state.data = Some(cb(&mut dialog, res_id));
                        }

                        // Drop the request
                        request.borrow_mut().take();

                        if let Some(waker) = state.waker.take() {
                            waker.wake();
                        }
                    }
                };

                GTK_MUTEX.run_locked(|| {
                    let mut state = state.lock().unwrap();
                    if gtk_init_check() {
                        state.dialog = Some(build());
                    }

                    if let Some(dialog) = &state.dialog {
                        unsafe {
                            let ptr = dialog.gtk_dialog_ptr();
                            gtk_sys::gtk_widget_show_all(ptr as *mut _);

                            connect_response(ptr as *mut _, callback);
                        }
                    } else {
                        state.data = Some(Default::default());
                    }
                });

                request.replace(Some(GTK_EVENT_HANDLER.request_iteration_start()));
            });
        }

        Self { state }
    }
}

impl<R, D> std::future::Future for GtkDialogFuture<R, D> {
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock().unwrap();

        if state.data.is_some() {
            Poll::Ready(state.data.take().unwrap())
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

use gobject_sys::GCallback;
use gtk_sys::{GtkDialog, GtkResponseType};
use std::ffi::c_void;
use std::os::raw::c_char;

unsafe fn connect_raw<F>(
    receiver: *mut gobject_sys::GObject,
    signal_name: *const c_char,
    trampoline: GCallback,
    closure: *mut F,
) {
    use std::mem;

    use glib_sys::gpointer;

    unsafe extern "C" fn destroy_closure<F>(ptr: *mut c_void, _: *mut gobject_sys::GClosure) {
        // destroy
        Box::<F>::from_raw(ptr as *mut _);
    }
    assert_eq!(mem::size_of::<*mut F>(), mem::size_of::<gpointer>());
    assert!(trampoline.is_some());
    let handle = gobject_sys::g_signal_connect_data(
        receiver,
        signal_name,
        trampoline,
        closure as *mut _,
        Some(destroy_closure::<F>),
        0,
    );
    assert!(handle > 0);
}

unsafe fn connect_response<F: Fn(GtkResponseType) + 'static>(dialog: *mut GtkDialog, f: F) {
    use std::mem::transmute;

    unsafe extern "C" fn response_trampoline<F: Fn(GtkResponseType) + 'static>(
        _this: *mut gtk_sys::GtkDialog,
        res: GtkResponseType,
        f: glib_sys::gpointer,
    ) {
        let f: &F = &*(f as *const F);

        f(res);
    }
    let f: Box<F> = Box::new(f);
    connect_raw(
        dialog as *mut _,
        b"response\0".as_ptr() as *const _,
        Some(transmute::<_, unsafe extern "C" fn()>(
            response_trampoline::<F> as *const (),
        )),
        Box::into_raw(f),
    );
}
