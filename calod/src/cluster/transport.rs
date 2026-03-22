use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

use super::error::NetworkError;

#[cfg(feature = "rdma")]
use ibverbs::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportType {
    /// InfiniBand
    InfiniBand,

    /// RoCEv1
    RoCEv1,

    /// RoCEv2
    RoCEv2,

    /// iWARP
    IWarp,

    /// Emulated (for testing or when hardware RDMA is not available)
    Emulated,
}

/// Device information
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub transport_type: TransportType,
    pub max_qp: u32,
    pub max_cq: u32,
    pub max_mr: u32,
}

/// Work request for RDMA operations
#[derive(Debug, Clone)]
pub enum WorkRequest {
    Send {
        wr_id: u64,
        data: Vec<u8>,
        immediate: Option<u32>,
    },
    Recv {
        wr_id: u64,
        buffer_size: usize,
    },
    Write {
        wr_id: u64,
        local_addr: u64,
        remote_addr: u64,
        length: usize,
        rkey: u32,
    },
    Read {
        wr_id: u64,
        local_addr: u64,
        remote_addr: u64,
        length: usize,
        rkey: u32,
    },
    AtomicCmpSwap {
        wr_id: u64,
        remote_addr: u64,
        compare: u64,
        swap: u64,
        rkey: u32,
    },
    AtomicFetchAdd {
        wr_id: u64,
        remote_addr: u64,
        add: u64,
        rkey: u32,
    },
}

/// Work completion status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStatus {
    Success,
    LocalLengthError,
    LocalQpOpError,
    LocalProtectionError,
    WrFlushError,
    MemoryWindowBindError,
    BadResponseError,
    LocalAccessError,
    RemoteInvalidRequestError,
    RemoteAccessError,
    RemoteOperationError,
    RetryExceededError,
    RnrRetryExceededError,
    TransportRetryCounterExceeded,
    Unknown,
}

/// Work completion
#[derive(Debug, Clone)]
pub struct WorkCompletion {
    pub wr_id: u64,
    pub status: CompletionStatus,
    pub opcode: WorkRequestOpcode,
    pub byte_len: u32,
    pub immediate: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkRequestOpcode {
    Send,
    Recv,
    Write,
    Read,
    AtomicCmpSwap,
    AtomicFetchAdd,
}

/// QP State
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QpState {
    Reset,
    Init,
    Rtr,  // Ready to Receive
    Rts,  // Ready to Send
    Sqd,  // Send Queue Drained
    Sqe,  // Send Queue Error
    Error,
}

/// RDMA transport implementation
pub struct RdmaTransport<'a> {
    /// Transport type
    transport_type: TransportType,

    /// Device name
    device_name: String,

    /// Device context - must be kept alive for PD
    #[cfg(feature = "rdma")]
    context: Option<Arc<Context>>,
    
    /// Protection domain handle (real or emulated)
    #[cfg(feature = "rdma")]
    pd: Option<Arc<ibverbs::ProtectionDomain<'a>>>,
    
    #[cfg(not(feature = "rdma"))]
    protection_domain_handle: u32,

    /// Next request ID
    next_request_id: AtomicU64,
}

/// Queue Pair for RDMA operations
pub struct QueuePair<'a> {
    /// Queue pair number
    qp_num: u32,

    /// Queue pair state
    state: QpState,

    /// Protection domain handle
    pd_handle: u32,

    /// Remote node (if connected)
    remote: Option<RemoteNode>,

    /// Send queue depth
    send_depth: u32,

    /// Receive queue depth
    recv_depth: u32,
    
    /// Send completion queue
    send_cq: Option<Arc<CompletionQueue<'a>>>,
    
    /// Receive completion queue
    recv_cq: Option<Arc<CompletionQueue<'a>>>,
    
    /// Real QP handle (when RDMA feature enabled)
    #[cfg(feature = "rdma")]
    qp_handle: Option<Arc<ibverbs::QueuePair<'a>>>,
    
    /// Emulated state
    #[cfg(not(feature = "rdma"))]
    emulated_state: EmulatedQpState,
}

/// Completion Queue for polling completions
pub struct CompletionQueue<'a> {
    cq_id: u32,
    capacity: u32,
    
    #[cfg(feature = "rdma")]
    cq_handle: Option<Arc<ibverbs::CompletionQueue<'a>>>,
    
    #[cfg(not(feature = "rdma"))]
    emulated_completions: std::sync::Mutex<Vec<WorkCompletion>>,
}

#[cfg(not(feature = "rdma"))]
#[derive(Debug)]
struct EmulatedQpState {
    pending_sends: Vec<WorkRequest>,
    pending_recvs: Vec<WorkRequest>,
}

#[derive(Debug, Clone)]
pub struct RemoteNode {
    pub id: String,
    pub address: String,
    pub qp_num: u32,
    pub lid: u16,
    pub gid: Option<[u8; 16]>,
}

/// QP attributes for connection
#[derive(Debug, Clone)]
pub struct QpAttributes {
    pub qp_num: u32,
    pub lid: u16,
    pub gid: Option<[u8; 16]>,
    pub psn: u32,  // Packet sequence number
}

impl<'a> RdmaTransport<'a> {
    /// Discover available RDMA devices
    pub fn discover_devices() -> Result<Vec<DeviceInfo>, NetworkError> {
        #[cfg(feature = "rdma")]
        {
            match ibverbs::devices() {
                Ok(devices) => {
                    let mut device_list = Vec::new();
                    for dev in &devices {
                        if let Some(name_cstr) = dev.name() {
                            let name = name_cstr.to_string_lossy().to_string();
                            device_list.push(DeviceInfo {
                                name,
                                transport_type: Self::detect_transport_type(&dev),
                                max_qp: 1024,
                                max_cq: 1024,
                                max_mr: 1024,
                            });
                        }
                    }
                    info!("Discovered {} RDMA devices", device_list.len());
                    Ok(device_list)
                }
                Err(e) => {
                    warn!("Failed to discover RDMA devices: {}, falling back to emulated", e);
                    Ok(vec![DeviceInfo {
                        name: "emulated".to_string(),
                        transport_type: TransportType::Emulated,
                        max_qp: 1024,
                        max_cq: 1024,
                        max_mr: 1024,
                    }])
                }
            }
        }
        
        #[cfg(not(feature = "rdma"))]
        {
            info!("RDMA feature not enabled, using emulated transport");
            Ok(vec![DeviceInfo {
                name: "emulated".to_string(),
                transport_type: TransportType::Emulated,
                max_qp: 1024,
                max_cq: 1024,
                max_mr: 1024,
            }])
        }
    }

    #[cfg(feature = "rdma")]
    fn detect_transport_type(device: &ibverbs::Device) -> TransportType {
        // Detect transport type based on device attributes
        // This is a simplified heuristic
        if let Some(name_cstr) = device.name() {
            let name = name_cstr.to_string_lossy();
            if name.contains("mlx") || name.contains("mthca") {
                return TransportType::InfiniBand;
            } else if name.contains("roce") {
                return TransportType::RoCEv2;
            } else if name.contains("iwarp") {
                return TransportType::IWarp;
            }
        }
        TransportType::InfiniBand  // Default
    }

    /// Create a new RDMA transport
    pub fn new(device_name: Option<String>) -> Result<Self, NetworkError> {
        let devices = Self::discover_devices()?;
        
        if devices.is_empty() {
            return Err(NetworkError::BindFailed("No RDMA devices available".to_string()));
        }
        
        let device_info = if let Some(name) = device_name {
            devices.into_iter()
                .find(|d| d.name == name)
                .ok_or_else(|| NetworkError::BindFailed(format!("Device {} not found", name)))?
        } else {
            devices.into_iter().next().unwrap()
        };
        
        info!("Using RDMA device: {} ({:?})", device_info.name, device_info.transport_type);
        
        #[cfg(feature = "rdma")]
        {
            let ctx = if device_info.transport_type != TransportType::Emulated {
                let devs = ibverbs::devices()
                    .map_err(|e| NetworkError::BindFailed(format!("Failed to get devices: {}", e)))?;
                let dev = devs.into_iter()
                    .find(|d| {
                        d.name()
                            .map(|n| n.to_string_lossy() == device_info.name.as_str())
                            .unwrap_or(false)
                    })
                    .ok_or_else(|| NetworkError::BindFailed("Device not found".to_string()))?;
                Some(Arc::new(dev.open()
                    .map_err(|e| NetworkError::BindFailed(format!("Failed to open device: {}", e)))?))
            } else {
                None
            };
            
            // Store context first, then create PD from it
            let mut transport = Self {
                transport_type: device_info.transport_type,
                device_name: device_info.name,
                context: ctx,
                pd: None,
                next_request_id: AtomicU64::new(1),
            };
            
            // Now create PD from the stored context
            if let Some(ref context) = transport.context {
                // Use a reference to the stored Arc - this won't create a self-reference
                // because ProtectionDomain internally uses raw pointers
                let pd_result = context.alloc_pd()
                    .map_err(|e| NetworkError::BindFailed(format!("Failed to create PD: {}", e)))?;
                transport.pd = Some(Arc::new(pd_result));
            }
            
            Ok(transport)
        }
        
        #[cfg(not(feature = "rdma"))]
        {
            Ok(Self {
                transport_type: TransportType::Emulated,
                device_name: device_info.name,
                protection_domain_handle: 1,
                next_request_id: AtomicU64::new(1),
            })
        }
    }

    /// Create a queue pair
    pub fn create_qp(&self, send_depth: u32, recv_depth: u32) -> Result<QueuePair, NetworkError> {
        let qp_num = self.next_request_id.fetch_add(1, Ordering::Relaxed) as u32;
        
        // TODO: Real RDMA QueuePair creation requires using QueuePairBuilder
        // For now, use emulated mode
        #[cfg(feature = "rdma")]
        {
            // Fallback to emulated for now
            Self::create_emulated_qp(qp_num, send_depth, recv_depth)
        }
        
        #[cfg(not(feature = "rdma"))]
        {
            Self::create_emulated_qp(qp_num, send_depth, recv_depth)
        }
    }
    
    fn create_emulated_qp(qp_num: u32, send_depth: u32, recv_depth: u32) -> Result<QueuePair<'a>, NetworkError> {
        debug!("Creating emulated QP {}", qp_num);
        Ok(QueuePair {
            qp_num,
            state: QpState::Reset,
            pd_handle: 1,
            remote: None,
            send_depth,
            recv_depth,
            send_cq: Some(Arc::new(CompletionQueue::new_emulated(send_depth * 2))),
            recv_cq: Some(Arc::new(CompletionQueue::new_emulated(recv_depth * 2))),
            #[cfg(feature = "rdma")]
            qp_handle: None,
            #[cfg(not(feature = "rdma"))]
            emulated_state: EmulatedQpState {
                pending_sends: Vec::new(),
                pending_recvs: Vec::new(),
            },
        })
    }
    
    /// Get transport type
    pub fn transport_type(&self) -> TransportType {
        self.transport_type
    }
}

impl<'a> QueuePair<'a> {
    pub fn new(qp_num: u32) -> Self { 
        Self { 
            qp_num, 
            state: QpState::Reset,
            pd_handle: 0, 
            remote: None, 
            send_depth: 256, 
            recv_depth: 256,
            send_cq: None,
            recv_cq: None,
            #[cfg(feature = "rdma")]
            qp_handle: None,
            #[cfg(not(feature = "rdma"))]
            emulated_state: EmulatedQpState {
                pending_sends: Vec::new(),
                pending_recvs: Vec::new(),
            },
        } 
    }
    
    /// Get QP number
    pub fn qp_num(&self) -> u32 {
        self.qp_num
    }
    
    /// Get QP state
    pub fn state(&self) -> QpState {
        self.state
    }
    
    /// Transition QP to INIT state
    pub fn to_init(&mut self, _port: u8, _access_flags: u32) -> Result<(), NetworkError> {
        // TODO: Real RDMA QP state transitions require using PreparedQueuePair::handshake
        // For now, just update the emulated state
        self.state = QpState::Init;
        debug!("QP {} transitioned to INIT", self.qp_num);
        Ok(())
    }
    
    /// Transition QP to RTR (Ready to Receive) state
    pub fn to_rtr(&mut self, remote: RemoteNode, _port: u8) -> Result<(), NetworkError> {
        // TODO: Real RDMA QP state transitions require using PreparedQueuePair::handshake
        // For now, just update the emulated state
        self.remote = Some(remote);
        self.state = QpState::Rtr;
        debug!("QP {} transitioned to RTR", self.qp_num);
        Ok(())
    }
    
    /// Transition QP to RTS (Ready to Send) state
    pub fn to_rts(&mut self) -> Result<(), NetworkError> {
        // TODO: Real RDMA QP state transitions require using PreparedQueuePair::handshake
        // For now, just update the emulated state
        self.state = QpState::Rts;
        debug!("QP {} transitioned to RTS", self.qp_num);
        Ok(())
    }
    
    /// Post a send work request
    pub fn post_send(&mut self, wr: WorkRequest) -> Result<(), NetworkError> {
        // TODO: Real RDMA send requires using QueuePair::post_send
        // For now, use emulated mode
        #[cfg(not(feature = "rdma"))]
        {
            self.emulated_state.pending_sends.push(wr.clone());
            // Simulate immediate completion
            if let Some(ref cq) = self.send_cq {
                cq.push_completion(WorkCompletion {
                    wr_id: match wr {
                        WorkRequest::Send { wr_id, .. } => wr_id,
                        WorkRequest::Write { wr_id, .. } => wr_id,
                        WorkRequest::Read { wr_id, .. } => wr_id,
                        _ => 0,
                    },
                    status: CompletionStatus::Success,
                    opcode: WorkRequestOpcode::Send,
                    byte_len: 0,
                    immediate: None,
                });
            }
        }
        Ok(())
    }
    
    /// Post a receive work request
    pub fn post_recv(&mut self, wr: WorkRequest) -> Result<(), NetworkError> {
        #[cfg(not(feature = "rdma"))]
        {
            self.emulated_state.pending_recvs.push(wr);
        }
        Ok(())
    }
    
    /// Poll for completions
    pub fn poll_completions(&self, max_completions: usize) -> Result<Vec<WorkCompletion>, NetworkError> {
        if let Some(ref cq) = self.send_cq {
            cq.poll(max_completions)
        } else {
            Ok(Vec::new())
        }
    }
}

impl<'a> CompletionQueue<'a> {
    // TODO: Real RDMA CompletionQueue creation requires using Context::create_cq
    // For now, removed to avoid compilation errors
    
    pub fn new_emulated(capacity: u32) -> Self {
        Self {
            cq_id: 0,
            capacity,
            #[cfg(feature = "rdma")]
            cq_handle: None,
            #[cfg(not(feature = "rdma"))]
            emulated_completions: std::sync::Mutex::new(Vec::new()),
        }
    }
    
    #[cfg(not(feature = "rdma"))]
    pub fn push_completion(&self, wc: WorkCompletion) {
        if let Ok(mut completions) = self.emulated_completions.lock() {
            completions.push(wc);
        }
    }
    
    /// Poll for completions
    pub fn poll(&self, max_completions: usize) -> Result<Vec<WorkCompletion>, NetworkError> {
        // TODO: Real RDMA polling requires using CompletionQueue::poll_cq
        // For now, use emulated mode only
        
        // Emulated mode
        #[cfg(not(feature = "rdma"))]
        {
            if let Ok(mut completions) = self.emulated_completions.lock() {
                let count = completions.len().min(max_completions);
                let result = completions.drain(..count).collect();
                return Ok(result);
            }
        }
        
        #[cfg(feature = "rdma")]
        {
            let _ = max_completions; // Suppress unused warning
        }
        
        Ok(Vec::new())
    }
}