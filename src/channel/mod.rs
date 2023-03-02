use std::hash::Hash;
use std::io;
use std::io::{Error, ErrorKind};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use crossbeam::atomic::AtomicCell;
use crossbeam::channel::Sender as PunchSender;
use crossbeam::epoch::Atomic;
use crossbeam::sync::Unparker;
use crossbeam_skiplist::map::Entry;
use crossbeam_skiplist::SkipMap;
use dashmap::DashMap;
use mio::{Events, Interest, Poll, Token, Waker};
use mio::net::UdpSocket as MioUdpSocket;
use parking_lot::{Mutex, RwLock};

use crate::channel::sender::Sender;
use crate::punch::{NatInfo, NatType};

pub mod sender;

#[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, Debug)]
pub struct Route {
    index: usize,
    addr: SocketAddr,
}

impl Route {
    pub fn new(index: usize,
               addr: SocketAddr, ) -> Self {
        Self {
            index,
            addr,
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub(crate) enum Status {
    Cone,
    Symmetric,
    Close,
}

pub struct Channel<ID> {
    channel_flag_gen: Arc<AtomicU64>,
    channel_flag: u64,
    src_default_udp: UdpSocket,
    default_udp: MioUdpSocket,
    share_info: Arc<RwLock<Vec<UdpSocket>>>,
    udp_list: Vec<MioUdpSocket>,
    direct_route_table: Arc<DashMap<ID, Route>>,
    direct_route_table_time: Arc<SkipMap<Route, (ID, AtomicI64, AtomicI64)>>,
    poll: Poll,
    change_waker_list: Arc<Mutex<Vec<(u64, Waker)>>>,
    events: Events,
    size: usize,
    status: Arc<AtomicCell<Status>>,
    cone_sender: PunchSender<(ID, NatInfo)>,
    symmetric_sender: PunchSender<(ID, NatInfo)>,
    lock: Arc<Mutex<()>>,
    un_parker: Unparker,
}

impl<ID> Drop for Channel<ID> {
    fn drop(&mut self) {
        let mut guard = self.change_waker_list.lock();
        let channel_flag = self.channel_flag;
        guard.retain(|(flag, _)| *flag != channel_flag);
    }
}

const DEFAULT_TOKEN_INDEX: usize = 10_0000;
const DEFAULT_TOKEN: Token = Token(DEFAULT_TOKEN_INDEX);
const CHANGE_TOKEN: Token = Token(10_0001);

impl<ID: Eq + Hash> Channel<ID> {
    pub(crate) fn new(size: usize, cone_sender: PunchSender<(ID, NatInfo)>,
                      symmetric_sender: PunchSender<(ID, NatInfo)>,
                      direct_route_table_time: Arc<SkipMap<Route, (ID, AtomicI64, AtomicI64)>>,
                      un_parker: Unparker, status: Arc<AtomicCell<Status>>) -> io::Result<Channel<ID>> {
        let channel_flag_gen = Arc::new(AtomicU64::new(1));
        let channel_flag = 0;
        let src_default_udp = UdpSocket::bind("0.0.0.0:0")?;
        src_default_udp.set_nonblocking(true)?;
        println!("{:?}", src_default_udp.local_addr()?);
        let mut default_udp = MioUdpSocket::from_std(src_default_udp.try_clone()?);
        let share_info = Arc::new(RwLock::new(Vec::with_capacity(size)));
        let mut udp_list = Vec::with_capacity(size);
        let direct_route_table = Arc::new(DashMap::with_capacity(64));
        let poll = Poll::new()?;
        let mut waker = Waker::new(poll.registry(), CHANGE_TOKEN)?;
        poll.registry().register(&mut default_udp, DEFAULT_TOKEN, Interest::READABLE)?;
        let mut change_waker_list = Vec::with_capacity(16);
        change_waker_list.push((channel_flag, waker));
        let change_waker_list = Arc::new(Mutex::new(change_waker_list));
        let events = Events::with_capacity(256);
        let size = size;
        let lock = Arc::new(Mutex::new(()));
        Ok(Channel {
            channel_flag_gen,
            channel_flag,
            src_default_udp,
            default_udp,
            share_info,
            udp_list,
            direct_route_table,
            direct_route_table_time,
            poll,
            change_waker_list,
            events,
            size,
            status,
            cone_sender,
            symmetric_sender,
            lock,
            un_parker,
        })
    }
    pub fn sender(&self) -> io::Result<Sender<ID>> {
        Ok(Sender {
            src_default_udp: self.src_default_udp.try_clone()?,
            share_info: self.share_info.clone(),
            direct_route_table: self.direct_route_table.clone(),
            direct_route_table_time: self.direct_route_table_time.clone(),
            status: self.status.clone(),
        })
    }
    pub fn try_clone(&self) -> io::Result<Channel<ID>> {
        let channel_flag_gen = self.channel_flag_gen.clone();
        let channel_flag = channel_flag_gen.fetch_add(1, Ordering::Relaxed);
        let src_default_udp = self.src_default_udp.try_clone()?;
        let mut default_udp = MioUdpSocket::from_std(src_default_udp.try_clone()?);
        let share_info = self.share_info.clone();
        let mut udp_list = Vec::with_capacity(self.size);
        let direct_route_table = self.direct_route_table.clone();
        let direct_route_table_time = self.direct_route_table_time.clone();
        let poll = Poll::new()?;
        let mut waker = Waker::new(poll.registry(), CHANGE_TOKEN)?;
        poll.registry().register(&mut default_udp, DEFAULT_TOKEN, Interest::READABLE)?;
        let change_waker_list = self.change_waker_list.clone();
        change_waker_list.lock().push((channel_flag, waker));
        let events = Events::with_capacity(256);
        let size = self.size;
        let status = self.status.clone();
        let cone_sender = self.cone_sender.clone();
        let symmetric_sender = self.symmetric_sender.clone();
        Channel::<ID>::change(&status, &share_info, &mut udp_list, &poll, size)?;
        let lock = self.lock.clone();
        let un_parker = self.un_parker.clone();
        let channel = Channel {
            channel_flag_gen,
            channel_flag,
            src_default_udp,
            default_udp,
            share_info,
            udp_list,
            direct_route_table,
            direct_route_table_time,
            poll,
            change_waker_list,
            events,
            size,
            status,
            cone_sender,
            symmetric_sender,
            lock,
            un_parker,
        };
        Ok(channel)
    }
}


impl<ID> Channel<ID> {
    #[inline]
    fn update_time(direct_route_table_time: &SkipMap<Route, (ID, AtomicI64, AtomicI64)>, route: &Route) {
        if let Some(time) = direct_route_table_time.get(&route) {
            time.value().1.store(chrono::Local::now().timestamp_millis(), Ordering::Relaxed);
        }
    }
    #[inline]
    fn udp_recv_(direct_route_table_time: &SkipMap<Route, (ID, AtomicI64, AtomicI64)>, udp: &MioUdpSocket, index: usize, buf: &mut [u8]) -> Option<io::Result<(usize, Route)>> {
        return match udp.recv_from(buf) {
            Ok((len, addr)) => {
                let route = Route::new(index, addr);
                Self::update_time(direct_route_table_time, &route);
                Some(Ok((len, Route::new(index, addr))))
            }
            Err(e) => {
                if e.kind() != ErrorKind::WouldBlock {
                    return Some(Err(e));
                }
                None
            }
        };
    }
    /// 接收数据
    /// 如果当前是对称网络，将监听一组udp socket，提高打洞效率
    pub fn recv_from(&mut self, buf: &mut [u8], timeout: Option<Duration>) -> io::Result<(usize, Route)> {
        loop {
            if let Some(rs) = Self::udp_recv_(&self.direct_route_table_time, &self.default_udp, DEFAULT_TOKEN_INDEX, buf) {
                return rs;
            }
            for index in 0..self.udp_list.len() {
                if let Some(rs) = Self::udp_recv_(&self.direct_route_table_time, &self.udp_list[index], index, buf) {
                    return rs;
                }
            }
            self.poll.poll(&mut self.events, timeout)?;
            for event in self.events.iter() {
                let (index, udp) = match event.token() {
                    DEFAULT_TOKEN => {
                        (DEFAULT_TOKEN_INDEX, &self.default_udp)
                    }
                    CHANGE_TOKEN => {
                        Channel::<ID>::change(&self.status, &self.share_info, &mut self.udp_list, &self.poll, self.size)?;
                        continue;
                    }
                    Token(index) => {
                        if let Some(udp) = self.udp_list.get(index) {
                            (index, udp)
                        } else {
                            continue;
                        }
                    }
                };
                if let Some(rs) = Self::udp_recv_(&self.direct_route_table_time, &udp, index, buf) {
                    return rs;
                }
            }
        }
    }

    fn change(status: &AtomicCell<Status>
              , share_info: &RwLock<Vec<UdpSocket>>, udp_list: &mut Vec<MioUdpSocket>, poll: &Poll, size: usize) -> io::Result<()> {
        match status.load() {
            Status::Cone => {
                let mut list = share_info.write();
                for udp in udp_list.iter_mut() {
                    let _ = poll.registry().deregister(udp);
                }
                udp_list.clear();
                list.clear();
                drop(list);
            }
            Status::Symmetric => {
                let mut list = share_info.write();
                for udp in udp_list.iter_mut() {
                    let _ = poll.registry().deregister(udp);
                }
                udp_list.clear();
                if list.is_empty() {
                    for _ in 0..size {
                        let udp = UdpSocket::bind("0.0.0.0:0")?;
                        // println!("list  {:?}", udp.local_addr()?);
                        udp.set_nonblocking(true)?;
                        list.push(udp);
                    }
                }
                let mut token = 0;
                for udp in list.iter() {
                    let mut mio_udp = MioUdpSocket::from_std(udp.try_clone()?);
                    let _ = poll.registry().register(&mut mio_udp, Token(token), Interest::READABLE);
                    udp_list.push(mio_udp);
                    token += 1;
                }
            }
            Status::Close => {
                return Err(Error::new(ErrorKind::Other, "channel close"));
            }
        }
        Ok(())
    }
}

impl<ID: Hash + Eq + Clone + Send + 'static> Channel<ID> {
    /// 添加路由
    pub fn add_route(&self, id: ID, route: Route) {
        let lock = self.lock.lock();
        self.direct_route_table.insert(id.clone(), route);
        let time = chrono::Local::now().timestamp_millis();
        self.direct_route_table_time.insert(route, (id, AtomicI64::new(time), AtomicI64::new(time)));
        drop(lock);
        self.un_parker.unpark();
    }
    /// 查询路由
    pub fn route(&self, id: ID) -> Option<Route> {
        self.direct_route_table.get(&id).map(|e| *e.value())
    }
    /// 删除路由
    pub fn remove_route(&self, id: ID) {
        let lock = self.lock.lock();
        if let Some((_, route)) = self.direct_route_table.remove(&id) {
            self.direct_route_table_time.remove(&route);
        }
        drop(lock);
    }
}

impl<ID: Hash + Eq + Clone> Channel<ID> {
    /// 发送到指定id
    pub fn send_to_id(&self, buf: &[u8], id: &ID) -> io::Result<usize> {
        match self.direct_route_table.get(id) {
            None => {
                Err(Error::new(ErrorKind::Other, "not fount"))
            }
            Some(e) => {
                self.send_to_route(buf, e.value())
            }
        }
    }
    /// 发送到指定路由
    pub fn send_to_route(&self, buf: &[u8], route: &Route) -> io::Result<usize> {
        if let Some(time) = self.direct_route_table_time.get(route) {
            let now = chrono::Local::now().timestamp_millis();
            time.value().2.store(now, Ordering::Relaxed);
        }
        if route.index == DEFAULT_TOKEN_INDEX {
            self.src_default_udp.send_to(buf, route.addr)
        } else {
            if let Some(udp) = self.udp_list.get(route.index) {
                udp.send_to(buf, route.addr)
            } else {
                Err(Error::new(ErrorKind::Other, "not fount"))
            }
        }
    }
    /// 发送到指定地址，将使用默认udpSocket发送
    pub fn send_to_addr(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.src_default_udp.send_to(buf, addr)
    }
    /// 添加到打洞队列
    pub fn punch(&self, peer_id: ID, nat_info: NatInfo) {
        match nat_info.nat_type {
            NatType::Symmetric => {
                let _ = self.symmetric_sender.try_send((peer_id, nat_info));
            }
            NatType::Cone => {
                let _ = self.cone_sender.try_send((peer_id, nat_info));
            }
        }
    }
    /// 设置当前设备所处的NAT类型
    pub fn set_nat_type(&self, nat_type: NatType) -> io::Result<()> {
        let guard = self.change_waker_list.lock();
        match self.status.load() {
            Status::Cone => {
                if nat_type == NatType::Symmetric {
                    self.status.store(Status::Symmetric);
                } else {
                    return Ok(());
                }
            }
            Status::Symmetric => {
                if nat_type == NatType::Cone {
                    self.status.store(Status::Cone);
                } else {
                    return Ok(());
                }
            }
            Status::Close => {
                return Err(Error::new(ErrorKind::Other, "closed"));
            }
        }
        for (_, waker) in guard.iter() {
            waker.wake()?;
        }
        Ok(())
    }
    pub fn nat_type(&self) -> io::Result<NatType> {
        match self.status.load() {
            Status::Cone => {
                Ok(NatType::Cone)
            }
            Status::Symmetric => {
                Ok(NatType::Symmetric)
            }
            Status::Close => {
                Err(Error::new(ErrorKind::Other, "closed"))
            }
        }
    }
    /// 关闭通道，recv将返回Err
    pub fn close(self) -> io::Result<()> {
        let guard = self.change_waker_list.lock();
        self.status.store(Status::Close);
        self.un_parker.unpark();
        for (_, waker) in guard.iter() {
            waker.wake()?;
        }
        Ok(())
    }
}
