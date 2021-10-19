//! 演示了如何实现一个（非常）基本的异步rust执行器和定时器。
//! 本文件的目的是提供一些关于各种构件如何结合的背景。

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};
// 一个允许我们实现`std::task::Waker`的工具，而不必使用`不安全`的代码。
use futures::task::{self, ArcWake};
// 用作排队预定任务的通道。
use crossbeam::channel;

// 主入口。一个mini-tokio实例被创建，一些任务被催生出来。
// 我们的mini-tokio实现只支持生成任务和设置延迟。
fn main() {
    // 创建mini-tokio实例.
    let mini_tokio = MiniTokio::new();

    // 产生根任务. 所有其他任务都是从这个根任务的上下文中产生的。
    // 在调用`mini_tokio.run()`之前，没有任何工作发生。
    mini_tokio.spawn(async {
        // Spawn a task
        spawn(async {
            // 等待一点时间，以便在 "hello "之后打印 "world"。
            delay(Duration::from_millis(100)).await;
            println!("world");
        });

        // Spawn a second task
        spawn(async {
            println!("hello");
        });

        // 我们还没有实现执行器关闭，所以要强制进程退出。
        delay(Duration::from_millis(200)).await;
        std::process::exit(0);
    });

    // 启动mini-tokio执行器循环。预定的任务被接收并执行。
    mini_tokio.run();
}

/// 一个非常基本的基于通道的期货执行器。
/// 当任务被唤醒时，它们被安排在通道的发送部分排队。
/// 执行者在接收端等待并执行收到的任务。
///
/// 当一个任务被执行时，通道的发送部分会通过任务的Waker传递。
struct MiniTokio {
    // 接收预定的任务。
    // 当一个任务被安排好后，相关的未来就可以取得进展了。
    // 这通常发生在任务使用的资源准备好进行操作的时候。
    // 例如，一个套接字收到了数据，一个`读'的调用将成功。
    scheduled: channel::Receiver<Arc<Task>>,

    // 调度测验的另一半发送者.
    sender: channel::Sender<Arc<Task>>,
}

impl MiniTokio {
    /// Initialize a new mini-tokio instance.
    fn new() -> MiniTokio {
        let (sender, scheduled) = channel::unbounded();

        MiniTokio { scheduled, sender }
    }

    /// 在mini-tokio实例上产生一个未来。
    ///
    /// 给定的未来将被包裹在 "任务 "线束中，并被推入 "调度 "队列。
    /// 当`run'被调用时，未来将被执行。
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Task::spawn(future, &self.sender);
    }

    /// 运行执行器。
    ///
    /// 这将启动执行器循环并无限期地运行。
    /// 没有实现关闭机制。
    ///
    /// 任务从 "调度"通道接收器中弹出。
    /// 在通道上接收到一个任务标志着该任务已经准备好被执行。
    /// 这发生在任务第一次被创建和它的唤醒者被使用时。
    fn run(&self) {
        // 设置CURRENT thread-local，使其指向当前的执行器。
        // Tokio使用线程本地变量来实现`tokio::spwn`。
        // 当进入运行时，执行器用线程-本地存储必要的上下文，以支持催生新任务。
        CURRENT.with(|cell| {
            *cell.borrow_mut() = Some(self.sender.clone());
        });

        // 执行者循环。预定的任务被接收。
        // 如果通道是空的，线程就会阻塞，直到有任务被接收。
        while let Ok(task) = self.scheduled.recv() {
            // 执行任务，直到它完成或无法取得进一步进展，并返回`Poll::Pending`。
            task.poll();
        }
    }
}

//相当于`tokio::spawn`。
// 当进入mini-tokio执行器时，`CURRENT`线程本地被设置为指向该执行器的通道的Send half。
// 然后，spwn需要为给定的`future`创建`Task`线束，并将其推入计划队列。
pub fn spawn<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    CURRENT.with(|cell| {
        let borrow = cell.borrow();
        let sender = borrow.as_ref().unwrap();
        Task::spawn(future, sender);
    });
}

// 与`thread::sleep`异步等效。在这个函数上的等待会在给定的时间内暂停。
//
// mini-tokio通过生成一个定时器线程来实现延迟，该线程在所要求的时间内睡眠，并在延迟完成后通知调用者。
// 每**次调用 "delay "就会产生一个线程。
// 这显然是一个糟糕的实现策略，没有人应该在生产中使用这个策略。
// Tokio并没有使用这种策略。
// 然而，它可以用几行代码来实现，所以我们在这里。
async fn delay(dur: Duration) {
    // `delay`是一个`叶子`的未来。有时，这被称为 "资源"。
    // 其他资源包括`套接字`和`通道`。
    // `资源`可能无法用`async/await`来实现，因为它们必须与一些操作系统的细节相结合。
    // 正因为如此，我们必须手动实现"未来"。
    //
    // 然而，将API暴露为`async fn'是很好的。一个有用的习惯是手动定义一个私有的未来，然后从一个公共的`async fn`API中使用它。
    struct Delay {
        // delay时长.
        when: Instant,
        // 延迟完成后通知的唤醒者。
        // 唤醒者必须能被定时器线程和未来线程访问，所以它被`Arc<Mutex<_>'包裹起来。
        waker: Option<Arc<Mutex<Waker>>>,
    }

    impl Future for Delay {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            // 首先，如果这是第一次调用future，则催生定时器线程。
            // 如果定时器线程已经在运行，确保存储的`Waker'与当前任务的Waker相匹配。
            if let Some(waker) = &self.waker {
                let mut waker = waker.lock().unwrap();

                // 检查存储的waker是否与当前任务的waker一致。
                // 这是必要的，因为在调用`poll'之间，`Delay'的未来实例可能会转移到不同的任务。
                // 如果发生这种情况，给定的`Context'所包含的waker就会不同，我们必须更新我们存储的waker以反映这种变化。
                if !waker.will_wake(cx.waker()) {
                    *waker = cx.waker().clone();
                }
            } else {
                let when = self.when;
                let waker = Arc::new(Mutex::new(cx.waker().clone()));
                self.waker = Some(waker.clone());

                // 这是第一次调用`poll`，催生定时器线程。
                thread::spawn(move || {
                    let now = Instant::now();

                    if now < when {
                        thread::sleep(when - now);
                    }

                    // 持续时间已经过了。通过调用唤醒器通知调用者。
                    let waker = waker.lock().unwrap();
                    waker.wake_by_ref();
                });
            }

            // 一旦唤醒者被存储起来，定时器线程被启动，就是检查延迟是否已经完成的时候了。
            // 这是通过检查当前的瞬间完成的。
            // 如果持续时间已经过了，那么未来就已经完成了，`Poll::Ready`将被返回。
            if Instant::now() >= self.when {
                Poll::Ready(())
            } else {
                // 持续时间没有过去，未来没有完成，所以返回`Poll::Pending`。
                //
                // `Future`特质契约要求，当返回`Pending`时，未来确保一旦未来应该再次轮询，就会向给定的唤醒者发出信号。
                // 在我们的例子中，通过在这里返回`Pending'，我们承诺一旦请求的持续时间结束，我们将调用包括在`Context'参数中的指定唤醒者。我们通过催生上面的定时器线程来确保这一点。
                //
                // 如果我们忘记调用唤醒器，任务将无限期地挂起。
                Poll::Pending
            }
        }
    }

    // Create an instance of our `Delay` future.
    let future = Delay {
        when: Instant::now() + dur,
        waker: None,
    };

    // Wait for the duration to complete.
    future.await;
}

// 用于跟踪当前的mini-tokio实例，以便`spawn'函数能够安排催生的任务。
thread_local! {
    static CURRENT: RefCell<Option<channel::Sender<Arc<Task>>>> =
        RefCell::new(None);
}

// 任务。包含未来以及未来被唤醒后安排的必要数据。
struct Task {
    // 未来被一个 "Mutex "包裹着，使 "任务 "结构 "同步"。
    // 只有一个线程试图使用`future`。
    // Tokio运行时通过使用 "不安全 "代码来避免mutex。盒子也被避免了。
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,

    // 当一个任务被通知时，它被排入这个通道。
    // 执行者会弹出被通知的任务并执行它们。
    executor: channel::Sender<Arc<Task>>,
}

impl Task {
    // Spawns a new taks with the given future.

    // 初始化一个新的包含给定未来的任务束，并将其推送给`sender`。通道的接收方将获得该任务并执行它。
    fn spawn<F>(future: F, sender: &channel::Sender<Arc<Task>>)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let task = Arc::new(Task {
            future: Mutex::new(Box::pin(future)),
            executor: sender.clone(),
        });

        let _ = sender.send(task);
    }

    // 执行一个计划任务。这将创建必要的`task::Context`，包含任务的waker。
    // 这个waker将任务推送到mini-redis计划通道上。然后用waker轮询未来。
    fn poll(self: Arc<Self>) {
        // Get a waker referencing the task.
        let waker = task::waker(self.clone());
        // Initialize the task context with the waker.
        let mut cx = Context::from_waker(&waker);

        // This will never block as only a single thread ever locks the future.
        let mut future = self.future.try_lock().unwrap();

        // Poll the future
        let _ = future.as_mut().poll(&mut cx);
    }
}

// 标准库提供了低级别的、不安全的API来定义wakers。
// 我们不用写不安全的代码，而是使用由`futures`板块提供的助手来定义一个能够安排我们的`Task`结构的waker。
impl ArcWake for Task {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        // 安排任务的执行。执行者从通道接收并轮询任务。
        let _ = arc_self.executor.send(arc_self.clone());
    }
}
