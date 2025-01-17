#![no_std]

// This must go FIRST so that all the other modules see its macros.
pub mod fmt;

use core::mem::MaybeUninit;
use core::sync::atomic::{compiler_fence, Ordering};

use ble::Ble;
use cmd::CmdPacket;
use embassy_hal_common::{into_ref, Peripheral, PeripheralRef};
use embassy_stm32::interrupt;
use embassy_stm32::interrupt::typelevel::Interrupt;
use embassy_stm32::ipcc::{Config, Ipcc, ReceiveInterruptHandler, TransmitInterruptHandler};
use embassy_stm32::peripherals::IPCC;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use evt::{CcEvt, EvtBox};
use mm::MemoryManager;
use sys::Sys;
use tables::{
    BleTable, DeviceInfoTable, Mac802_15_4Table, MemManagerTable, RefTable, SysTable, ThreadTable, TracesTable,
};
use unsafe_linked_list::LinkedListNode;

pub mod ble;
pub mod channels;
pub mod cmd;
pub mod consts;
pub mod evt;
pub mod mm;
pub mod shci;
pub mod sys;
pub mod tables;
pub mod unsafe_linked_list;

#[link_section = "TL_REF_TABLE"]
pub static mut TL_REF_TABLE: MaybeUninit<RefTable> = MaybeUninit::uninit();

#[link_section = "MB_MEM1"]
static mut TL_DEVICE_INFO_TABLE: MaybeUninit<DeviceInfoTable> = MaybeUninit::uninit();

#[link_section = "MB_MEM1"]
static mut TL_BLE_TABLE: MaybeUninit<BleTable> = MaybeUninit::uninit();

#[link_section = "MB_MEM1"]
static mut TL_THREAD_TABLE: MaybeUninit<ThreadTable> = MaybeUninit::uninit();

#[link_section = "MB_MEM1"]
static mut TL_SYS_TABLE: MaybeUninit<SysTable> = MaybeUninit::uninit();

#[link_section = "MB_MEM1"]
static mut TL_MEM_MANAGER_TABLE: MaybeUninit<MemManagerTable> = MaybeUninit::uninit();

#[link_section = "MB_MEM1"]
static mut TL_TRACES_TABLE: MaybeUninit<TracesTable> = MaybeUninit::uninit();

#[link_section = "MB_MEM1"]
static mut TL_MAC_802_15_4_TABLE: MaybeUninit<Mac802_15_4Table> = MaybeUninit::uninit();

#[link_section = "MB_MEM2"]
static mut FREE_BUF_QUEUE: MaybeUninit<LinkedListNode> = MaybeUninit::uninit();

// Not in shared RAM
static mut LOCAL_FREE_BUF_QUEUE: MaybeUninit<LinkedListNode> = MaybeUninit::uninit();

#[allow(dead_code)] // Not used currently but reserved
#[link_section = "MB_MEM2"]
static mut TRACES_EVT_QUEUE: MaybeUninit<LinkedListNode> = MaybeUninit::uninit();

type PacketHeader = LinkedListNode;

const TL_PACKET_HEADER_SIZE: usize = core::mem::size_of::<PacketHeader>();
const TL_EVT_HEADER_SIZE: usize = 3;
const TL_CS_EVT_SIZE: usize = core::mem::size_of::<evt::CsEvt>();

#[link_section = "MB_MEM2"]
static mut CS_BUFFER: MaybeUninit<[u8; TL_PACKET_HEADER_SIZE + TL_EVT_HEADER_SIZE + TL_CS_EVT_SIZE]> =
    MaybeUninit::uninit();

#[link_section = "MB_MEM2"]
static mut EVT_QUEUE: MaybeUninit<LinkedListNode> = MaybeUninit::uninit();

#[link_section = "MB_MEM2"]
static mut SYSTEM_EVT_QUEUE: MaybeUninit<LinkedListNode> = MaybeUninit::uninit();

#[link_section = "MB_MEM2"]
pub static mut SYS_CMD_BUF: MaybeUninit<CmdPacket> = MaybeUninit::uninit();

/**
 * Queue length of BLE Event
 * This parameter defines the number of asynchronous events that can be stored in the HCI layer before
 * being reported to the application. When a command is sent to the BLE core coprocessor, the HCI layer
 * is waiting for the event with the Num_HCI_Command_Packets set to 1. The receive queue shall be large
 * enough to store all asynchronous events received in between.
 * When CFG_TLBLE_MOST_EVENT_PAYLOAD_SIZE is set to 27, this allow to store three 255 bytes long asynchronous events
 * between the HCI command and its event.
 * This parameter depends on the value given to CFG_TLBLE_MOST_EVENT_PAYLOAD_SIZE. When the queue size is to small,
 * the system may hang if the queue is full with asynchronous events and the HCI layer is still waiting
 * for a CC/CS event, In that case, the notification TL_BLE_HCI_ToNot() is called to indicate
 * to the application a HCI command did not receive its command event within 30s (Default HCI Timeout).
 */
const CFG_TLBLE_EVT_QUEUE_LENGTH: usize = 5;
const CFG_TLBLE_MOST_EVENT_PAYLOAD_SIZE: usize = 255;
const TL_BLE_EVENT_FRAME_SIZE: usize = TL_EVT_HEADER_SIZE + CFG_TLBLE_MOST_EVENT_PAYLOAD_SIZE;

const fn divc(x: usize, y: usize) -> usize {
    ((x) + (y) - 1) / (y)
}

const POOL_SIZE: usize = CFG_TLBLE_EVT_QUEUE_LENGTH * 4 * divc(TL_PACKET_HEADER_SIZE + TL_BLE_EVENT_FRAME_SIZE, 4);

#[link_section = "MB_MEM2"]
static mut EVT_POOL: MaybeUninit<[u8; POOL_SIZE]> = MaybeUninit::uninit();

#[link_section = "MB_MEM2"]
static mut SYS_SPARE_EVT_BUF: MaybeUninit<[u8; TL_PACKET_HEADER_SIZE + TL_EVT_HEADER_SIZE + 255]> =
    MaybeUninit::uninit();

#[link_section = "MB_MEM2"]
static mut BLE_SPARE_EVT_BUF: MaybeUninit<[u8; TL_PACKET_HEADER_SIZE + TL_EVT_HEADER_SIZE + 255]> =
    MaybeUninit::uninit();

#[link_section = "MB_MEM2"]
static mut BLE_CMD_BUFFER: MaybeUninit<CmdPacket> = MaybeUninit::uninit();

#[link_section = "MB_MEM2"]
//                                 fuck these "magic" numbers from ST ---v---v
static mut HCI_ACL_DATA_BUFFER: MaybeUninit<[u8; TL_PACKET_HEADER_SIZE + 5 + 251]> = MaybeUninit::uninit();

// TODO: remove these items

#[allow(dead_code)]
/// current event that is produced during IPCC IRQ handler execution
/// on SYS channel
static EVT_CHANNEL: Channel<CriticalSectionRawMutex, EvtBox, 32> = Channel::new();

#[allow(dead_code)]
/// last received Command Complete event
static LAST_CC_EVT: Signal<CriticalSectionRawMutex, CcEvt> = Signal::new();

static STATE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

pub struct TlMbox<'d> {
    _ipcc: PeripheralRef<'d, IPCC>,

    pub sys_subsystem: Sys,
    pub mm_subsystem: MemoryManager,
    pub ble_subsystem: Ble,
}

impl<'d> TlMbox<'d> {
    pub fn init(
        ipcc: impl Peripheral<P = IPCC> + 'd,
        _irqs: impl interrupt::typelevel::Binding<interrupt::typelevel::IPCC_C1_RX, ReceiveInterruptHandler>
            + interrupt::typelevel::Binding<interrupt::typelevel::IPCC_C1_TX, TransmitInterruptHandler>,
        config: Config,
    ) -> Self {
        into_ref!(ipcc);

        unsafe {
            TL_REF_TABLE.as_mut_ptr().write_volatile(RefTable {
                device_info_table: TL_DEVICE_INFO_TABLE.as_ptr(),
                ble_table: TL_BLE_TABLE.as_ptr(),
                thread_table: TL_THREAD_TABLE.as_ptr(),
                sys_table: TL_SYS_TABLE.as_ptr(),
                mem_manager_table: TL_MEM_MANAGER_TABLE.as_ptr(),
                traces_table: TL_TRACES_TABLE.as_ptr(),
                mac_802_15_4_table: TL_MAC_802_15_4_TABLE.as_ptr(),
                // zigbee_table: TL_ZIGBEE_TABLE.as_ptr(),
                // lld_tests_table: TL_LLD_TESTS_TABLE.as_ptr(),
                // ble_lld_table: TL_BLE_LLD_TABLE.as_ptr(),
            });

            TL_SYS_TABLE
                .as_mut_ptr()
                .write_volatile(MaybeUninit::zeroed().assume_init());
            TL_DEVICE_INFO_TABLE
                .as_mut_ptr()
                .write_volatile(MaybeUninit::zeroed().assume_init());
            TL_BLE_TABLE
                .as_mut_ptr()
                .write_volatile(MaybeUninit::zeroed().assume_init());
            TL_THREAD_TABLE
                .as_mut_ptr()
                .write_volatile(MaybeUninit::zeroed().assume_init());
            TL_MEM_MANAGER_TABLE
                .as_mut_ptr()
                .write_volatile(MaybeUninit::zeroed().assume_init());

            TL_TRACES_TABLE
                .as_mut_ptr()
                .write_volatile(MaybeUninit::zeroed().assume_init());
            TL_MAC_802_15_4_TABLE
                .as_mut_ptr()
                .write_volatile(MaybeUninit::zeroed().assume_init());
            //            TL_ZIGBEE_TABLE
            //                .as_mut_ptr()
            //                .write_volatile(MaybeUninit::zeroed().assume_init());
            //            TL_LLD_TESTS_TABLE
            //                .as_mut_ptr()
            //                .write_volatile(MaybeUninit::zeroed().assume_init());
            //            TL_BLE_LLD_TABLE
            //                .as_mut_ptr()
            //                .write_volatile(MaybeUninit::zeroed().assume_init());

            EVT_POOL
                .as_mut_ptr()
                .write_volatile(MaybeUninit::zeroed().assume_init());
            SYS_SPARE_EVT_BUF
                .as_mut_ptr()
                .write_volatile(MaybeUninit::zeroed().assume_init());
            BLE_SPARE_EVT_BUF
                .as_mut_ptr()
                .write_volatile(MaybeUninit::zeroed().assume_init());

            {
                BLE_CMD_BUFFER
                    .as_mut_ptr()
                    .write_volatile(MaybeUninit::zeroed().assume_init());
                HCI_ACL_DATA_BUFFER
                    .as_mut_ptr()
                    .write_volatile(MaybeUninit::zeroed().assume_init());
                CS_BUFFER
                    .as_mut_ptr()
                    .write_volatile(MaybeUninit::zeroed().assume_init());
            }
        }

        compiler_fence(Ordering::SeqCst);

        Ipcc::enable(config);

        let sys = sys::Sys::new();
        let ble = ble::Ble::new();
        let mm = mm::MemoryManager::new();

        // enable interrupts
        interrupt::typelevel::IPCC_C1_RX::unpend();
        interrupt::typelevel::IPCC_C1_TX::unpend();

        unsafe { interrupt::typelevel::IPCC_C1_RX::enable() };
        unsafe { interrupt::typelevel::IPCC_C1_TX::enable() };

        STATE.reset();

        Self {
            _ipcc: ipcc,
            sys_subsystem: sys,
            ble_subsystem: ble,
            mm_subsystem: mm,
        }
    }
}
