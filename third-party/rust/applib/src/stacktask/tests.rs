use super::*;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::Wake;
struct Count(AtomicUsize);
impl Wake for Count {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
struct Later {
    polls: usize,
    value: u64,
}
impl Future for Later {
    type Output = u64;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u64> {
        if self.polls == 0 {
            Poll::Ready(self.value)
        } else {
            self.polls -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}
#[test]
fn nested_sync_stack_waits_without_pumping_other_futures() {
    #[inline(never)]
    fn deep(s: &Suspender, n: usize) -> u64 {
        let local = [n as u64; 64];
        let value = if n == 0 {
            s.wait(Later {
                polls: 3,
                value: 17,
            })
            .unwrap()
        } else {
            deep(s, n - 1)
        };
        assert!(local.iter().all(|&v| v == n as u64));
        value + n as u64
    }
    // SAFETY: Vierentwintig frames van 512 bytes passen ruim in 128 KiB;
    // annulering wordt in deze test niet gevraagd, geen longjmp of herintrede.
    let task = unsafe { Task::new(128 << 10, |s| deep(s, 24)) }.unwrap();
    let mut task = pin!(task);
    let calls = Arc::new(Count(AtomicUsize::new(0)));
    let waker = Waker::from(calls.clone());
    let mut cx = Context::from_waker(&waker);
    for _ in 0..3 {
        assert!(task.as_mut().poll(&mut cx).is_pending());
    }
    assert_eq!(calls.0.load(Ordering::Relaxed), 3);
    assert_eq!(task.as_mut().poll(&mut cx), Poll::Ready(317));
}
#[test]
fn cancellation_drops_future_and_sync_locals_before_stack_release() {
    struct Mark<'a>(&'a AtomicUsize, usize);
    impl Drop for Mark<'_> {
        fn drop(&mut self) {
            self.0.fetch_or(self.1, Ordering::Relaxed);
        }
    }
    let state = AtomicUsize::new(0);
    {
        // SAFETY: Begrensde testframes; na Cancelled keert de closure direct terug.
        let mut task = unsafe {
            Task::new(65536, |s| {
                let _local = Mark(&state, 1);
                let pending = async {
                    let _future = Mark(&state, 2);
                    core::future::pending::<()>().await;
                };
                assert_eq!(s.wait(pending), Err(Cancelled));
                state.fetch_or(4, Ordering::Relaxed);
            })
        }
        .unwrap();
        assert!(
            Pin::new(&mut task)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(state.load(Ordering::Relaxed), 0);
    }
    assert_eq!(state.load(Ordering::Relaxed), 7);
}
#[test]
fn dropping_a_suspended_task_finishes_cleanup_and_unstarted_task_never_runs() {
    let state = AtomicUsize::new(0);
    {
        // SAFETY: Begrensde closure retourneert bij de eerste annulering.
        let mut task = unsafe {
            Task::new(65536, |s| {
                if s.wait(core::future::pending::<()>()).is_err() {
                    state.store(1, Ordering::Relaxed);
                }
            })
        }
        .unwrap();
        assert!(
            Pin::new(&mut task)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    assert_eq!(state.load(Ordering::Relaxed), 1);
    {
        // SAFETY: Geen uitvoering in deze test, de constructor krijgt een geldige maat.
        let _task = unsafe { Task::new(65536, |_| state.store(2, Ordering::Relaxed)) }.unwrap();
    }
    assert_eq!(state.load(Ordering::Relaxed), 1);
}
#[test]
fn many_switches_preserve_floating_point_and_integer_callee_state() {
    // SAFETY: Een kleine lus zonder recursie; bij annulering stopt hij meteen.
    let mut task = unsafe {
        Task::new(65536, |s| {
            let (mut a, mut b) = (1.25f64, 0x123456789abcdef0u64);
            for i in 0..1000 {
                if s.wait(Later { polls: 1, value: i }).is_err() {
                    return None;
                }
                a += 0.125;
                b = b.rotate_left(7) ^ i;
            }
            Some((a, b))
        })
    }
    .unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    let mut b = 0x123456789abcdef0u64;
    for i in 0..1000 {
        assert!(Pin::new(&mut task).poll(&mut cx).is_pending());
        b = b.rotate_left(7) ^ i;
    }
    assert_eq!(
        Pin::new(&mut task).poll(&mut cx),
        Poll::Ready(Some((126.25, b)))
    );
}
