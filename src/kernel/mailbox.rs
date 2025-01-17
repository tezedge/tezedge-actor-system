use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

use crate::{
    actor::actor_cell::ExtendedCell,
    actor::*,
    kernel::{
        queue::{queue, EnqueueResult, QueueEmpty, QueueReader, QueueWriter},
        Dock,
    },
    system::ActorCreated,
    system::{ActorSystem, SystemEvent, SystemMsg},
    AnyMessage, Envelope, Message,
};

pub trait MailboxSchedule {
    fn set_scheduled(&self, b: bool);

    fn is_scheduled(&self) -> bool;
}

#[derive(Debug)]
pub struct AnyEnqueueError;

impl From<()> for AnyEnqueueError {
    fn from(_: ()) -> AnyEnqueueError {
        AnyEnqueueError
    }
}

pub trait AnySender: Send + Sync {
    fn try_any_enqueue(&self, msg: &mut AnyMessage, sender: Sender) -> Result<(), AnyEnqueueError>;

    fn set_sched(&self, b: bool);

    fn is_sched(&self) -> bool;
}

#[derive(Clone)]
pub struct MailboxSender<Msg: Message> {
    queue: QueueWriter<Msg>,
    scheduled: Arc<AtomicBool>,
}

impl<Msg> MailboxSender<Msg>
where
    Msg: Message,
{
    pub fn try_enqueue(&self, msg: Envelope<Msg>) -> EnqueueResult<Msg> {
        self.queue.try_enqueue(msg)
    }
}

impl<Msg> MailboxSchedule for MailboxSender<Msg>
where
    Msg: Message,
{
    fn set_scheduled(&self, b: bool) {
        self.scheduled.store(b, Ordering::Relaxed);
    }

    fn is_scheduled(&self) -> bool {
        self.scheduled.load(Ordering::Relaxed)
    }
}

impl<Msg> AnySender for MailboxSender<Msg>
where
    Msg: Message,
{
    fn try_any_enqueue(&self, msg: &mut AnyMessage, sender: Sender) -> Result<(), AnyEnqueueError> {
        let actual = msg.take().map_err(|_| AnyEnqueueError)?;
        let msg = Envelope {
            msg: actual,
            sender,
        };
        self.try_enqueue(msg).map_err(|_| AnyEnqueueError)
    }

    fn set_sched(&self, b: bool) {
        self.set_scheduled(b)
    }

    fn is_sched(&self) -> bool {
        self.is_scheduled()
    }
}

#[derive(Clone)]
pub struct Mailbox<Msg: Message> {
    inner: Arc<MailboxInner<Msg>>,
}

pub struct MailboxInner<Msg: Message> {
    msg_process_limit: u32,
    queue: QueueReader<Msg>,
    sys_queue: QueueReader<SystemMsg>,
    suspended: Arc<AtomicBool>,
    scheduled: Arc<AtomicBool>,
}

impl<Msg: Message> Mailbox<Msg> {
    pub fn try_dequeue(&self) -> Result<Envelope<Msg>, QueueEmpty> {
        self.inner.queue.try_dequeue()
    }

    pub fn sys_try_dequeue(&self) -> Result<Envelope<SystemMsg>, QueueEmpty> {
        self.inner.sys_queue.try_dequeue()
    }

    pub fn has_msgs(&self) -> bool {
        self.inner.queue.has_msgs()
    }

    pub fn has_sys_msgs(&self) -> bool {
        self.inner.sys_queue.has_msgs()
    }

    pub fn set_suspended(&self, b: bool) {
        self.inner.suspended.store(b, Ordering::Relaxed);
    }

    fn is_suspended(&self) -> bool {
        self.inner.suspended.load(Ordering::Relaxed)
    }

    fn msg_process_limit(&self) -> u32 {
        self.inner.msg_process_limit
    }
}

impl<Msg> MailboxSchedule for Mailbox<Msg>
where
    Msg: Message,
{
    fn set_scheduled(&self, b: bool) {
        self.inner.scheduled.store(b, Ordering::Relaxed);
    }

    fn is_scheduled(&self) -> bool {
        self.inner.scheduled.load(Ordering::Relaxed)
    }
}

pub fn mailbox<Msg>(
    msg_process_limit: u32,
) -> (MailboxSender<Msg>, MailboxSender<SystemMsg>, Mailbox<Msg>)
where
    Msg: Message,
{
    let (qw, qr) = queue::<Msg>();
    let (sqw, sqr) = queue::<SystemMsg>();

    let scheduled = Arc::new(AtomicBool::new(false));

    let sender = MailboxSender {
        queue: qw,
        scheduled: scheduled.clone(),
    };

    let sys_sender = MailboxSender {
        queue: sqw,
        scheduled: scheduled.clone(),
    };

    let mailbox = MailboxInner {
        msg_process_limit,
        queue: qr,
        sys_queue: sqr,
        suspended: Arc::new(AtomicBool::new(true)),
        scheduled,
    };

    let mailbox = Mailbox {
        inner: Arc::new(mailbox),
    };

    (sender, sys_sender, mailbox)
}

pub fn run_mailbox<A>(mbox: &Mailbox<A::Msg>, ctx: Context<A::Msg>, dock: &mut Dock<A>)
where
    A: Actor,
{
    let sen = Sentinel {
        actor: ctx.myself().into(),
        parent: ctx.myself().parent(),
        mbox,
    };

    let mut actor = dock.actor.lock().unwrap().take();
    let cell = &mut dock.cell;

    process_sys_msgs(sen.mbox, &ctx, cell, &mut actor);

    if actor.is_some() && !sen.mbox.is_suspended() {
        process_msgs(sen.mbox, &ctx, cell, &mut actor);
    }

    process_sys_msgs(sen.mbox, &ctx, cell, &mut actor);

    if actor.is_some() {
        let mut a = dock.actor.lock().unwrap();
        *a = actor;
    }

    sen.mbox.set_scheduled(false);

    let has_msgs = sen.mbox.has_msgs() || sen.mbox.has_sys_msgs();
    if has_msgs && !sen.mbox.is_scheduled() {
        ctx.kernel.schedule();
    }
}

fn process_msgs<A>(
    mbox: &Mailbox<A::Msg>,
    ctx: &Context<A::Msg>,
    cell: &ExtendedCell<A::Msg>,
    actor: &mut Option<A>,
) where
    A: Actor,
{
    let mut count = 0;

    loop {
        if count < mbox.msg_process_limit() {
            match mbox.try_dequeue() {
                Ok(msg) => {
                    let (msg, sender) = (msg.msg, msg.sender);
                    actor.as_mut().unwrap().recv(ctx, msg, sender);
                    process_sys_msgs(mbox, ctx, cell, actor);

                    count += 1;
                }
                Err(_) => {
                    break;
                }
            }
        } else {
            break;
        }
    }
}

fn process_sys_msgs<A>(
    mbox: &Mailbox<A::Msg>,
    ctx: &Context<A::Msg>,
    cell: &ExtendedCell<A::Msg>,
    actor: &mut Option<A>,
) where
    A: Actor,
{
    // All system messages are processed in this mailbox execution
    // and we prevent any new messages that have since been added to the queue
    // from being processed by staging them in a Vec.
    // This prevents during actor restart.
    let mut sys_msgs: Vec<Envelope<SystemMsg>> = Vec::new();
    while let Ok(sys_msg) = mbox.sys_try_dequeue() {
        sys_msgs.push(sys_msg);
    }

    for msg in sys_msgs {
        match msg.msg {
            SystemMsg::ActorInit => handle_init(mbox, ctx, cell, actor),
            SystemMsg::Command(cmd) => cell.receive_cmd(cmd, actor),
            SystemMsg::Event(evt) => handle_evt(evt, ctx, cell, actor),
            SystemMsg::Failed(failed) => handle_failed(failed, cell),
        }
    }
}

fn handle_init<A>(
    mbox: &Mailbox<A::Msg>,
    ctx: &Context<A::Msg>,
    cell: &ExtendedCell<A::Msg>,
    actor: &mut Option<A>,
) where
    A: Actor,
{
    actor.as_mut().unwrap().pre_start(ctx);
    mbox.set_suspended(false);

    if cell.is_user() {
        ctx.system.publish_event(
            ActorCreated {
                actor: cell.myself().into(),
            }
            .into(),
        );
    }

    actor.as_mut().unwrap().post_start(ctx);
}

fn handle_failed<Msg>(failed: BasicActorRef, cell: &ExtendedCell<Msg>)
where
    Msg: Message,
{
    cell.handle_failure(failed)
}

fn handle_evt<A>(
    evt: SystemEvent,
    ctx: &Context<A::Msg>,
    cell: &ExtendedCell<A::Msg>,
    actor: &mut Option<A>,
) where
    A: Actor,
{
    if actor.is_some() {
        actor
            .as_mut()
            .unwrap()
            .sys_recv(ctx, SystemMsg::Event(evt.clone()), None);
    }

    if let SystemEvent::ActorTerminated(terminated) = evt {
        cell.death_watch(&terminated.actor, actor);
    }
}

struct Sentinel<'a, Msg: Message> {
    parent: BasicActorRef,
    actor: BasicActorRef,
    mbox: &'a Mailbox<Msg>,
}

impl<'a, Msg> Drop for Sentinel<'a, Msg>
where
    Msg: Message,
{
    fn drop(&mut self) {
        if thread::panicking() {
            // Suspend the mailbox to prevent further message processing
            self.mbox.set_suspended(true);

            // There is no actor to park but kernel still needs to mark as no longer scheduled
            // self.kernel.park_actor(self.actor.uri.uid, None);
            self.mbox.set_scheduled(false);

            // Message the parent (this failed actor's supervisor) to decide how to handle the failure
            self.parent.sys_tell(SystemMsg::Failed(self.actor.clone()));
        }
    }
}

pub fn flush_to_deadletters<Msg>(mbox: &Mailbox<Msg>, actor: &BasicActorRef, sys: &ActorSystem)
where
    Msg: Message,
{
    while let Ok(Envelope { msg, sender }) = mbox.try_dequeue() {
        let dl = DeadLetter {
            msg: format!("{:?}", msg),
            sender,
            recipient: actor.clone(),
        };

        sys.dead_letters().tell(
            Publish {
                topic: "dead_letter".into(),
                msg: dl,
            },
            None,
        );
    }
}

#[derive(Clone, Debug)]
pub struct MailboxConfig {
    pub msg_process_limit: u32,
}

impl Default for MailboxConfig {
    fn default() -> Self {
        MailboxConfig {
            msg_process_limit: 1000,
        }
    }
}

impl MailboxConfig {
    // Option<()> allow to use ? for parsing toml value, ignore it
    pub fn merge(&mut self, v: &toml::Value) -> Option<()> {
        let v = v.as_table()?;
        let msg_process_limit = v.get("msg_process_limit")?.as_integer()?;
        self.msg_process_limit = msg_process_limit as u32;
        None
    }
}
