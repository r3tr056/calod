use std::{collections::HashMap, ptr::NonNull, sync::{atomic::{AtomicUsize, Ordering}, Arc}};

use tokio::sync::RwLock;
use dashmap::DashMap;
use tracing::{debug, info};
use uuid::Uuid;

use super::security::{EncryptionContext, SecurityManager};
use super::config::RdmaConfig;
use super::error::NetworkError;
use super::transport::TransportType;

#[cfg(feature = "rdma")]
use ibverbs;

// (Future) RDMA specific imports removed in scaffold; using placeholder design.

pub struct MemoryRegion<'a> {
    /// Memory region id
    id: Uuid,

    /// Memory pointer and length
    ptr: NonNull<u8>,
    len: usize,

    /// Memory registration key
    rkey: u32,

    /// local key
    lkey: u32,

    /// Access permissions
    permissions: MemoryPermissions,

    /// Memory protection domain
    domain: Arc<ProtectionDomain<'a>>,

    /// Reference count
    ref_count: AtomicUsize,

    encryption_context: Option<Arc<EncryptionContext>>,

    // Opaque handle to underlying registered region (real ibverbs when feature enabled)
    _mr_handle: Option<usize>,
}

unsafe impl<'a> Send for MemoryRegion<'a> {}
unsafe impl<'a> Sync for MemoryRegion<'a> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPermissions {
    pub local_read: bool,
    pub local_write: bool,
    pub remote_read: bool,
    pub remote_write: bool,
    pub remote_atomic: bool,
}

impl MemoryPermissions {
    /// Default permissions for local operations only
    pub fn local_only() -> Self {
        Self {
            local_read: true,
            local_write: true,
            remote_read: false,
            remote_write: false,
            remote_atomic: false,
        }
    }
    
    /// Permissions for remote read operations
    pub fn remote_read() -> Self {
        Self {
            local_read: true,
            local_write: true,
            remote_read: true,
            remote_write: false,
            remote_atomic: false,
        }
    }
    
    /// Permissions for remote read/write operations
    pub fn remote_read_write() -> Self {
        Self {
            local_read: true,
            local_write: true,
            remote_read: true,
            remote_write: true,
            remote_atomic: false,
        }
    }
    
    /// Full permissions including atomic operations
    pub fn full() -> Self {
        Self {
            local_read: true,
            local_write: true,
            remote_read: true,
            remote_write: true,
            remote_atomic: true,
        }
    }
    
    /// Convert to ibverbs access flags
    #[cfg(feature = "rdma")]
    pub fn to_ibverbs_flags(&self) -> ibverbs::ibv_access_flags {
        let mut flags = ibverbs::ibv_access_flags(0);
        if self.local_write { flags = flags | ibverbs::ibv_access_flags::IBV_ACCESS_LOCAL_WRITE; }
        if self.remote_read { flags = flags | ibverbs::ibv_access_flags::IBV_ACCESS_REMOTE_READ; }
        if self.remote_write { flags = flags | ibverbs::ibv_access_flags::IBV_ACCESS_REMOTE_WRITE; }
        if self.remote_atomic { flags = flags | ibverbs::ibv_access_flags::IBV_ACCESS_REMOTE_ATOMIC; }
        flags
    }
    
    /// Check if operation is allowed
    pub fn allows(&self, operation: MemoryOperation) -> bool {
        match operation {
            MemoryOperation::LocalRead => self.local_read,
            MemoryOperation::LocalWrite => self.local_write,
            MemoryOperation::RemoteRead => self.remote_read,
            MemoryOperation::RemoteWrite => self.remote_write,
            MemoryOperation::RemoteAtomic => self.remote_atomic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOperation {
    LocalRead,
    LocalWrite,
    RemoteRead,
    RemoteWrite,
    RemoteAtomic,
}

pub struct ProtectionDomain<'a> {
    id: Uuid,
    device_name: String,
    
    #[cfg(feature = "rdma")]
    pd_handle: Option<Arc<ibverbs::ProtectionDomain<'a>>>,
    
    #[cfg(not(feature = "rdma"))]
    pd_handle: Option<usize>,
}

impl<'a> ProtectionDomain<'a> {
    #[cfg(feature = "rdma")]
    pub fn new(context: &'a Arc<ibverbs::Context>) -> Result<Self, NetworkError> {
        let pd = Arc::new(context.alloc_pd()
            .map_err(|e| NetworkError::BindFailed(format!("Failed to create PD: {}", e)))?);
        
        Ok(Self {
            id: Uuid::new_v4(),
            device_name: "rdma_device".to_string(),
            pd_handle: Some(pd),
        })
    }
    
    #[cfg(not(feature = "rdma"))]
    pub fn new_emulated(device_name: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            device_name,
            pd_handle: Some(1),
        }
    }
    
    pub fn id(&self) -> &Uuid {
        &self.id
    }
}


pub struct MemoryRegistrar<'a> {
    domain: Arc<ProtectionDomain<'a>>,
    regions: DashMap<Uuid, Arc<MemoryRegion<'a>>>,
    next_rkey: AtomicUsize,
}

impl<'a> MemoryRegistrar<'a> {
    pub fn new(domain: Arc<ProtectionDomain<'a>>) -> Self {
        Self {
            domain,
            regions: DashMap::new(),
            next_rkey: AtomicUsize::new(1),
        }
    }
    
    /// Register a memory region
    pub fn register(
        &self,
        ptr: NonNull<u8>,
        len: usize,
        permissions: MemoryPermissions,
        encryption_context: Option<Arc<EncryptionContext>>,
    ) -> Result<Arc<MemoryRegion<'a>>, NetworkError> {
        let rkey = self.next_rkey.fetch_add(1, Ordering::SeqCst) as u32;
        let lkey = rkey;  // Simplified: same key for local and remote
        
        #[cfg(feature = "rdma")]
        {
            if let Some(ref pd) = self.domain.pd_handle {
                // Register with real RDMA
                let access_flags = permissions.to_ibverbs_flags();
                let mr_handle = unsafe {
                    // Real RDMA registration would happen here
                    // ibverbs::MemoryRegion::register(pd, ptr.as_ptr() as *mut _, len, access_flags)
                    None  // Placeholder
                };
                
                let region = Arc::new(MemoryRegion::new(
                    ptr,
                    len,
                    rkey,
                    lkey,
                    permissions,
                    self.domain.clone(),
                    encryption_context,
                    mr_handle,
                ));
                
                self.regions.insert(region.id, region.clone());
                info!("Registered RDMA memory region {} ({}B, rkey={})", region.id, len, rkey);
                return Ok(region);
            }
        }
        
        // Emulated mode
        let region = Arc::new(MemoryRegion::new(
            ptr,
            len,
            rkey,
            lkey,
            permissions,
            self.domain.clone(),
            encryption_context,
            None,
        ));
        
        self.regions.insert(region.id, region.clone());
        debug!("Registered emulated memory region {} ({}B, rkey={})", region.id, len, rkey);
        Ok(region)
    }
    
    /// Deregister a memory region
    pub fn deregister(&self, region_id: &Uuid) -> Result<(), NetworkError> {
        if let Some((_, region)) = self.regions.remove(region_id) {
            // Drop will handle cleanup
            debug!("Deregistered memory region {}", region_id);
            Ok(())
        } else {
            Err(NetworkError::Protocol(format!("Region {} not found", region_id)))
        }
    }
    
    /// Get a memory region by ID
    pub fn get_region(&self, region_id: &Uuid) -> Option<Arc<MemoryRegion<'a>>> {
        self.regions.get(region_id).map(|r| r.clone())
    }
    
    /// List all registered regions
    pub fn list_regions(&self) -> Vec<Arc<MemoryRegion<'a>>> {
        self.regions.iter().map(|r| r.clone()).collect()
    }
}

pub struct GlobalMemoryManager<'a> {
    pub config: Arc<RdmaConfig>,
    pub security_manager: Arc<SecurityManager>,
    pub registrar: Arc<MemoryRegistrar<'a>>,
    pub remote_regions: DashMap<Uuid, RemoteMemoryRegion>,
    pub global_memory_map: RwLock<GlobalMemoryMap>,
    transport_type: TransportType,
}

impl<'a> GlobalMemoryManager<'a> {
    pub fn new(
        config: Arc<RdmaConfig>,
        security_manager: Arc<SecurityManager>,
        domain: Arc<ProtectionDomain<'a>>,
        transport_type: TransportType,
    ) -> Self {
        Self {
            config,
            security_manager,
            registrar: Arc::new(MemoryRegistrar::new(domain)),
            remote_regions: DashMap::new(),
            global_memory_map: RwLock::new(GlobalMemoryMap {
                segments: HashMap::new(),
                node_segments: HashMap::new(),
            }),
            transport_type,
        }
    }
    
    /// Allocate and register local memory
    pub fn allocate_local(
        &self,
        size: usize,
        permissions: MemoryPermissions,
    ) -> Result<Arc<MemoryRegion<'a>>, NetworkError> {
        // Allocate aligned memory
        let layout = std::alloc::Layout::from_size_align(size, 4096)
            .map_err(|e| NetworkError::BindFailed(format!("Invalid layout: {}", e)))?;
        
        let ptr = unsafe {
            let raw_ptr = std::alloc::alloc(layout);
            if raw_ptr.is_null() {
                return Err(NetworkError::BindFailed("Memory allocation failed".to_string()));
            }
            NonNull::new_unchecked(raw_ptr)
        };
        
        // Register with RDMA
        let encryption_context = if self.config.default_allow_remote_read {
            None  // Could add encryption here
        } else {
            None
        };
        
        self.registrar.register(ptr, size, permissions, encryption_context)
    }
    
    /// Register remote memory region
    pub fn register_remote(
        &self,
        node_id: String,
        region_id: Uuid,
        rkey: u32,
        addr: u64,
        len: usize,
        permissions: MemoryPermissions,
    ) -> Result<(), NetworkError> {
        let remote_region = RemoteMemoryRegion {
            node_id: node_id.clone(),
            rkey,
            addr,
            len,
            permissions,
        };
        
        self.remote_regions.insert(region_id, remote_region);
        debug!("Registered remote memory region {} from node {}", region_id, node_id);
        Ok(())
    }
    
    /// Get remote memory region info
    pub fn get_remote_region(&self, region_id: &Uuid) -> Option<RemoteMemoryRegion> {
        self.remote_regions.get(region_id).map(|r| r.clone())
    }
    
    /// Publish local region to global directory
    pub async fn publish_local_region(
        &self,
        region: Arc<MemoryRegion<'a>>,
        node_id: String,
    ) -> Result<(), NetworkError> {
        let segment = MemorySegment {
            id: region.id,
            node_id: node_id.clone(),
            remote_region: RemoteMemoryRegion {
                node_id: node_id.clone(),
                rkey: region.rkey,
                addr: region.ptr.as_ptr() as u64,
                len: region.len,
                permissions: region.permissions,
            },
            size: region.len,
        };
        
        let mut map = self.global_memory_map.write().await;
        map.segments.insert(region.id, segment);
        map.node_segments
            .entry(node_id)
            .or_insert_with(Vec::new)
            .push(region.id);
        
        info!("Published memory region {} to global directory", region.id);
        Ok(())
    }
    
    /// Lookup memory segment by key hash
    pub async fn lookup_segment(&self, segment_id: &Uuid) -> Option<MemorySegment> {
        let map = self.global_memory_map.read().await;
        map.segments.get(segment_id).cloned()
    }
    
    /// Get all segments for a node
    pub async fn get_node_segments(&self, node_id: &str) -> Vec<Uuid> {
        let map = self.global_memory_map.read().await;
        map.node_segments
            .get(node_id)
            .cloned()
            .unwrap_or_default()
    }
    
    /// Validate memory access permissions
    pub fn validate_access(
        &self,
        region_id: &Uuid,
        operation: MemoryOperation,
    ) -> Result<(), NetworkError> {
        // Check local region
        if let Some(region) = self.registrar.get_region(region_id) {
            if region.permissions.allows(operation) {
                return Ok(());
            } else {
                return Err(NetworkError::Protocol(
                    format!("Operation {:?} not allowed on region {}", operation, region_id)
                ));
            }
        }
        
        // Check remote region
        if let Some(remote) = self.remote_regions.get(region_id) {
            if remote.permissions.allows(operation) {
                return Ok(());
            } else {
                return Err(NetworkError::Protocol(
                    format!("Operation {:?} not allowed on remote region {}", operation, region_id)
                ));
            }
        }
        
        Err(NetworkError::Protocol(format!("Region {} not found", region_id)))
    }
}

#[derive(Debug, Clone)]
pub struct RemoteMemoryRegion {
    pub node_id: String,
    pub rkey: u32,
    pub addr: u64,
    pub len: usize,
    pub permissions: MemoryPermissions,
}

/// Global memory map for distributed addressing
#[derive(Debug)]
pub struct GlobalMemoryMap {
    /// Memory segments by ID
    pub segments: HashMap<Uuid, MemorySegment>,
    
    /// Node to memory segment mapping
    pub node_segments: HashMap<String, Vec<Uuid>>,
}

/// Memory segment in the global memory map
#[derive(Debug, Clone)]
pub struct MemorySegment {
    pub id: Uuid,
    pub node_id: String,
    pub remote_region: RemoteMemoryRegion,
    pub size: usize,
}


impl<'a> MemoryRegion<'a> {
    fn new(
        ptr: NonNull<u8>,
        len: usize,
        rkey: u32,
        lkey: u32,
        permissions: MemoryPermissions,
        domain: Arc<ProtectionDomain<'a>>,
        encryption_context: Option<Arc<EncryptionContext>>,
        mr: Option<usize>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            ptr,
            len,
            rkey,
            lkey,
            permissions,
            domain,
            ref_count: AtomicUsize::new(1),
            encryption_context,
            _mr_handle: mr,
        }
    }
    
    /// Get region ID
    pub fn id(&self) -> &Uuid {
        &self.id
    }
    
    /// Get remote key
    pub fn rkey(&self) -> u32 {
        self.rkey
    }
    
    /// Get local key
    pub fn lkey(&self) -> u32 {
        self.lkey
    }
    
    /// Get memory address
    pub fn addr(&self) -> u64 {
        self.ptr.as_ptr() as u64
    }
    
    /// Get length
    pub fn len(&self) -> usize {
        self.len
    }
    
    /// Get permissions
    pub fn permissions(&self) -> MemoryPermissions {
        self.permissions
    }
    
    /// Increment reference count
    pub fn add_ref(&self) {
        self.ref_count.fetch_add(1, Ordering::SeqCst);
    }
}

impl<'a> Drop for MemoryRegion<'a> {
    fn drop(&mut self) {
        if self.ref_count.fetch_sub(1, Ordering::SeqCst) == 1 {
            debug!("dropping memory region {:?}", self.id);
            // Actual deregistration would occur here when mr goes out of scope.
        }
    }
}