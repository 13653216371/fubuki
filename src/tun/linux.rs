use std::cell::UnsafeCell;
use std::future::Future;
use std::net::Ipv4Addr;

use ahash::{HashSet, HashSetExt};
use anyhow::{anyhow, Result};
use ipnet::{IpNet, Ipv4Net};
use netconfig::Interface;
use parking_lot::Mutex;
use tun::Device;

use crate::tun::TunDevice;

pub struct Linuxtun {
    ips: Mutex<HashSet<Ipv4Addr>>,
    fd: UnsafeCell<tun::AsyncDevice>,
    inter: Interface
}

unsafe impl Sync for Linuxtun {}

pub fn create() -> Result<Linuxtun> {
    let mut config = tun::Configuration::default();

    config.queues(2);

    config.platform(|config| {
        config.packet_information(false);
        config.napi(std::env::var("FUBUKI_USE_IFF_NAPI").is_ok());
        config.vnet_hdr(std::env::var("FUBUKI_USE_IFF_VNET_HDR").is_ok());
    }).up();

    let device = tun::create_as_async(&config)?;
    let device_name = device.get_ref().name()?;
    let inter = netconfig::Interface::try_from_name(&device_name).map_err(|e| anyhow!(e.to_string()))?;

    Ok(Linuxtun {
        ips: Mutex::new(HashSet::new()),
        fd: UnsafeCell::new(device),
        inter,
    })
}

impl TunDevice for Linuxtun {
    type SendFut<'a> = impl Future<Output = Result<()>> + 'a;
    type RecvFut<'a> = impl Future<Output = Result<usize>> + 'a;

    fn send_packet<'a>(&'a self, packet: &'a [u8]) -> Self::SendFut<'a> {
        let fd = unsafe { &*self.fd.get() };

        async {
            // todo add from ip address message
            const INVALID_ARGUMENT: i32 = 22;

            let res = fd.send(packet).await;

            match res {
                Err(e) if e.raw_os_error() == Some(INVALID_ARGUMENT) => {
                    let f = || {
                        let src = crate::common::net::get_ip_src_addr(packet)?;
                        let dst = crate::common::net::get_ip_dst_addr(packet)?;
                        Result::<_, anyhow::Error>::Ok((src, dst))
                    };

                    let (src, dst) = match f() {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("tun: {}", e);
                            return Ok(())
                        }
                    };

                    warn!("tun: send packet to tun warn: {}; packet {}->{}", e, src, dst);
                    Ok(())
                }
                res => res.map(|_| ()).map_err(|e| anyhow!(e))
            }
        }
    }

    fn recv_packet<'a>(&'a self, buff: &'a mut [u8]) -> Self::RecvFut<'a> {
        let fd = unsafe { &*self.fd.get() };

        async {
            let len = fd.recv(buff).await?;
            Ok(len)
        }
    }

    fn set_mtu(&self, mtu: usize) -> Result<()> {
        let fd = unsafe { &mut *self.fd.get() };
        fd.get_mut().set_mtu(mtu as i32)?;
        Ok(())
    }

    fn add_addr(&self, addr: Ipv4Addr, netmask: Ipv4Addr) -> Result<()> {
        let mut guard = self.ips.lock();

        if guard.contains(&addr) {
            return Ok(());
        }

        self.inter
            .add_address(IpNet::V4(Ipv4Net::with_netmask(addr, netmask)?))
            .map_err(|e| anyhow!(e.to_string()))?;

        guard.insert(addr);
        Ok(())
    }

    fn delete_addr(&self, addr: Ipv4Addr, netmask: Ipv4Addr) -> Result<()> {
        let mut guard = self.ips.lock();

        if !guard.contains(&addr) {
            return Ok(());
        }

        self.inter
            .remove_address(IpNet::V4(Ipv4Net::with_netmask(addr, netmask)?))
            .map_err(|e| anyhow!(e.to_string()))?;

        guard.remove(&addr);
        Ok(())
    }

    fn get_index(&self) -> u32 {
        self.inter.index().expect("can't get interface index")
    }
}
