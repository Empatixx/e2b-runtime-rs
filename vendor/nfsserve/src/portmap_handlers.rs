use crate::context::RPCContext;
use crate::portmap;
use crate::rpc::*;
use crate::xdr::*;
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::cast::FromPrimitive;
use std::io::{Read, Write};
use tracing::{debug, error};

/*
 From RFC 1057 Appendix A

 program PMAP_PROG {
    version PMAP_VERS {
       void PMAPPROC_NULL(void)         = 0;
       bool PMAPPROC_SET(mapping)       = 1;
       bool PMAPPROC_UNSET(mapping)     = 2;
       unsigned int PMAPPROC_GETPORT(mapping)   = 3;
       pmaplist PMAPPROC_DUMP(void)         = 4;
       call_result PMAPPROC_CALLIT(call_args)  = 5;
    } = 2;
 } = 100000;
*/

#[allow(non_camel_case_types)]
#[allow(clippy::upper_case_acronyms)]
#[derive(Copy, Clone, Debug, FromPrimitive, ToPrimitive)]
enum PortmapProgram {
    PMAPPROC_NULL = 0,
    PMAPPROC_SET = 1,
    PMAPPROC_UNSET = 2,
    PMAPPROC_GETPORT = 3,
    PMAPPROC_DUMP = 4,
    PMAPPROC_CALLIT = 5,
    INVALID,
}

pub fn handle_portmap(
    xid: u32,
    call: call_body,
    input: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    if call.vers != portmap::VERSION {
        error!(
            "Invalid Portmap Version number {} != {}",
            call.vers,
            portmap::VERSION
        );
        prog_mismatch_reply_message(xid, portmap::VERSION).serialize(output)?;
        return Ok(());
    }
    let prog = PortmapProgram::from_u32(call.proc).unwrap_or(PortmapProgram::INVALID);

    match prog {
        PortmapProgram::PMAPPROC_NULL => pmapproc_null(xid, input, output)?,
        PortmapProgram::PMAPPROC_GETPORT => pmapproc_getport(xid, input, output, context)?,
        _ => {
            proc_unavail_reply_message(xid).serialize(output)?;
        }
    }
    Ok(())
}

pub fn pmapproc_null(
    xid: u32,
    _: &mut impl Read,
    output: &mut impl Write,
) -> Result<(), anyhow::Error> {
    debug!("pmapproc_null({:?}) ", xid);
    // build an RPC reply
    let msg = make_success_reply(xid);
    debug!("\t{:?} --> {:?}", xid, msg);
    msg.serialize(output)?;
    Ok(())
}

/*
 * We fake a portmapper here. And always direct back to the same host port
 */
pub fn pmapproc_getport(
    xid: u32,
    read: &mut impl Read,
    output: &mut impl Write,
    context: &RPCContext,
) -> Result<(), anyhow::Error> {
    let mut mapping = portmap::mapping::default();
    mapping.deserialize(read)?;
    debug!("pmapproc_getport({:?}, {:?}) ", xid, mapping);
    make_success_reply(xid).serialize(output)?;
    let port = registered_port(&mapping, context.local_port as u32);
    debug!("\t{:?} --> {:?}", xid, port);
    port.serialize(output)?;
    Ok(())
}

const NFS3_VERSION: u32 = 3;

fn registered_port(mapping: &portmap::mapping, port: u32) -> u32 {
    let program = mapping.prog == crate::nfs::PROGRAM || mapping.prog == crate::mount::PROGRAM;
    match program && mapping.vers == NFS3_VERSION && mapping.prot == portmap::IPPROTO_TCP {
        true => port,
        false => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(prog: u32, vers: u32, prot: u32) -> u32 {
        registered_port(&portmap::mapping { prog, vers, prot, port: 0 }, 2049)
    }

    #[test]
    fn only_nfs_and_mountd_v3_over_tcp_are_registered() {
        assert_eq!(lookup(100003, 3, portmap::IPPROTO_TCP), 2049);
        assert_eq!(lookup(100005, 3, portmap::IPPROTO_TCP), 2049);
        assert_eq!(lookup(100005, 1, portmap::IPPROTO_TCP), 0);
        assert_eq!(lookup(100003, 3, portmap::IPPROTO_UDP), 0);
        assert_eq!(lookup(100021, 4, portmap::IPPROTO_TCP), 0);
    }
}
