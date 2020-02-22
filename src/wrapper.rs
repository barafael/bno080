use crate::interface::{
    SensorInterface,
    PACKET_HEADER_LENGTH};
use embedded_hal::{
    blocking::delay::{ DelayMs},
};

use core::ops::{Shr};


const PACKET_SEND_BUF_LEN: usize = 256;
const PACKET_RECV_BUF_LEN: usize = 1024;

const NUM_CHANNELS: usize = 6;

#[derive(Debug)]
pub enum WrapperError<E> {

    CommError(E),

    /// Invalid chip ID was read
    InvalidChipId(u8),
    /// Unsupported sensor firmware version
    InvalidFWVersion(u8),
}

pub struct BNO080<SI> {
    pub(crate) sensor_interface: SI,
    /// each communication channel with the device has its own sequence number
    sequence_numbers: [u8; NUM_CHANNELS],
    /// buffer for building and sending packet to the sensor hub
    packet_send_buf: [u8; PACKET_SEND_BUF_LEN],
    /// buffer for building packets received from the sensor hub
    packet_recv_buf: [u8; PACKET_RECV_BUF_LEN],

    /// has the device been succesfully reset
    device_reset: bool,
    /// has the product ID been verified
    prod_id_verified: bool,

}


impl<SI> BNO080<SI> {

    pub fn new_with_interface(sensor_interface: SI) -> Self {
        Self {
            sensor_interface,
            sequence_numbers: [0; NUM_CHANNELS],
            packet_send_buf: [0; PACKET_SEND_BUF_LEN],
            packet_recv_buf: [0; PACKET_RECV_BUF_LEN],
            device_reset: false,
            prod_id_verified: false
        }
    }
}

impl<SI, SE> BNO080<SI>
    where
        SI: SensorInterface<SensorError = SE>,
{
    /// Receive and ignore one message
    pub fn eat_one_message(&mut self) -> usize {
        let res = self.receive_packet();
        res.unwrap_or(0)
    }

    /// Consume all available messages on the port without processing them
    pub fn eat_all_messages(&mut self, delay: &mut dyn DelayMs<u8>) {
        loop {
            let received_len = self.eat_one_message();
            if received_len == 0 {
                break;
            } else {
                //give some time to other parts of the system
                delay.delay_ms(1);
            }
        }
    }

    /// return the number of messages handled
    pub fn handle_one_message(&mut self) -> u32 {
        let mut msg_count = 0;

        let res = self.receive_packet();
        if res.is_ok() {
            let received_len = res.unwrap_or(0);
            if received_len > 0 {
                msg_count += 1;
                self.handle_received_packet(received_len);
            }
        }

        msg_count
    }


    fn handle_advertise_response(&mut self, received_len: usize) {
        let payload_len = received_len - PACKET_HEADER_LENGTH;
        let payload = &self.packet_recv_buf[PACKET_HEADER_LENGTH..received_len];
        let mut cursor:usize = 1; //skip response type

        while cursor < payload_len {
            let _tag: u8 = payload[cursor]; cursor += 1;
            let len: u8 = payload[cursor]; cursor +=1;
            //let val: u8 = payload + cursor;
            cursor += len as usize;
        }

    }

    // Sensor input reports have the form:
    // [u8; 5]  timestamp in microseconds
    // u8 report ID
    // u8 sequence number of report
    // ?? follows: about 5 * 2 bytes for eg rotation vec
    fn handle_input_report(&mut self, received_len: usize) {
        let msg = &self.packet_recv_buf[..received_len];
        let mut cursor = PACKET_HEADER_LENGTH; //skip header
        cursor += 5; // skip timestamp
        let feature_report_id = msg[cursor];
        //cursor += 1;

        match feature_report_id {
            SENSOR_REPORTID_ROTATION_VECTOR => {
                //iprintln!("SENSOR_REPORTID_ROTATION_VECTOR").unwrap();
            },
            _ => {
                //iprintln!("handle_input_report[{}]: 0x{:01x} ", received_len, feature_report_id).unwrap();
            }
        }
    }

    pub fn handle_received_packet(&mut self, received_len: usize) {
        let msg = &self.packet_recv_buf[..received_len];
        let chan_num =  msg[2];
        //let _seq_num =  msg[3];
        let report_id: u8 = msg[4];

        match chan_num {
            CHANNEL_SENSOR_REPORTS => {
                self.handle_input_report(received_len);
            },
            SHTP_CHAN_COMMAND => {
                match report_id {
                    0 => { //RESP_ADVERTISE
                        self.handle_advertise_response(received_len);
                    },
                    _ => {

                    }
                }
            },
            CHANNEL_EXECUTABLE => {
                match report_id {
                    EXECUTABLE_DEVICE_RESP_RESET_COMPLETE => {
                        self.device_reset = true;
                    },
                    _ => {

                    }
                }
            },
            CHANNEL_HUB_CONTROL => {
                match report_id {
                    SENSORHUB_COMMAND_RESP => {
                        let cmd_resp = msg[6];
                        if cmd_resp == SH2_STARTUP_INIT_UNSOLICITED {

                        }
                        else {
                        }
                    },
                    SENSORHUB_PROD_ID_RESP => {
                        self.prod_id_verified = true;
                    },
                    _ =>  {

                    }
                }
            },
            _ => {

            }
        }

    }

    /// The BNO080 starts up with all sensors disabled,
    /// waiting for the application to configure it.
    pub fn init(&mut self, delay_source: &mut impl DelayMs<u8>) -> Result<(), WrapperError<SE>> {
        //Section 5.1.1.1 : On system startup, the SHTP control application will send
        // its full advertisement response, unsolicited, to the host.

        self.sensor_interface.setup( Some(delay_source)).map_err(WrapperError::CommError)?;
        self.soft_reset()?;
        delay_source.delay_ms(50);
        self.eat_one_message();
        delay_source.delay_ms(50);
        self.eat_all_messages(delay_source);
        // delay.delay_ms(50);
        // self.eat_all_messages(delay);

        self.verify_product_id()?;

        Ok(())
    }

    /// Tell the sensor to start reporting the fused rotation vector
    /// on a regular cadence. Note that the maximum valid update rate
    /// is 1 kHz, based on the max update rate of the sensor's gyros.
    pub fn enable_rotation_vector(&mut self, millis_between_reports: u16) -> Result<(), WrapperError<SE>> {
        self.enable_report(SENSOR_REPORTID_ROTATION_VECTOR, millis_between_reports)
    }

    /// Enable a particular report
    fn enable_report(&mut self, report_id: u8, millis_between_reports: u16) -> Result<(), WrapperError<SE>> {
        let micros_between_reports: u32 = (millis_between_reports as u32) * 1000;
        let cmd_body: [u8; 17] = [
            SHTP_REPORT_SET_FEATURE_COMMAND,
            report_id,
            0, //feature flags
            0, //LSB change sensitivity
            0, //MSB change sensitivity
            (micros_between_reports & 0xFFu32) as u8, // LSB report interval, microseconds
            (micros_between_reports.shr(8) & 0xFFu32) as u8,
            (micros_between_reports.shr(16) & 0xFFu32) as u8,
            (micros_between_reports.shr(24) & 0xFFu32) as u8, // MSB report interval
            0, // LSB Batch Interval
            0,
            0,
            0, // MSB Batch interval
            0, // LSB sensor-specific config
            0,
            0,
            0, // MSB sensor-specific config
        ];

        self.send_packet(CHANNEL_HUB_CONTROL, &cmd_body)?;
        Ok(())
    }

    fn send_packet(&mut self, channel: u8, body_data: &[u8]) -> Result<usize, WrapperError<SE>> {
        let body_len = body_data.len();

        self.sequence_numbers[channel as usize] += 1;
        let packet_length = body_len + PACKET_HEADER_LENGTH;
        let packet_header = [
            (packet_length & 0xFF) as u8, //LSB
            packet_length.shr(8) as u8, //MSB
            channel,
            self.sequence_numbers[channel as usize]
        ];

        self.packet_send_buf[..PACKET_HEADER_LENGTH].copy_from_slice(packet_header.as_ref());
        self.packet_send_buf[PACKET_HEADER_LENGTH..packet_length].copy_from_slice(body_data);
        self.sensor_interface
            .send_packet( &self.packet_send_buf[..packet_length])
            .map_err(WrapperError::CommError)?;
        Ok(packet_length)
    }

    /// Read one packet into the receive buffer
    pub fn receive_packet(&mut self) -> Result<usize, WrapperError<SE>> {
        self.packet_recv_buf[0] = 0;
        self.packet_recv_buf[1] = 0;
        
        let packet_len = self.sensor_interface
            .read_packet(&mut self.packet_recv_buf)
            .map_err(WrapperError::CommError)?;

        Ok(packet_len)
    }

    fn verify_product_id(&mut self) -> Result<(), WrapperError<SE> > {
        let cmd_body: [u8; 2] = [
            SENSORHUB_PROD_ID_REQ, //request product ID
            0, //reserved
        ];

        let recv_len = self.send_and_receive_packet(CHANNEL_HUB_CONTROL, cmd_body.as_ref())?;

        //verify the response
        if recv_len > PACKET_HEADER_LENGTH {
            //iprintln!("resp: {:?}", &self.msg_buf[..recv_len]).unwrap();
            let report_id = self.packet_recv_buf[PACKET_HEADER_LENGTH + 0];
            if SENSORHUB_PROD_ID_RESP == report_id {
                self.prod_id_verified = true;
                return Ok(())
            }
        }

        return Err(WrapperError::InvalidChipId(0));
    }

    pub fn soft_reset(&mut self) -> Result<(), WrapperError<SE>> {
        let data:[u8; 1] = [EXECUTABLE_DEVICE_CMD_RESET]; //reset execute
        // send command packet and ignore received packets
        self.send_packet(CHANNEL_EXECUTABLE, data.as_ref())?;
        Ok(())
    }

    /// Send a packet and receive the response
    fn send_and_receive_packet(&mut self, channel: u8, body_data: &[u8]) ->  Result<usize, WrapperError<SE>> {
        //TODO reimplement with WriteRead once that interface is stable
        self.send_packet(channel, body_data)?;
        self.receive_packet()
    }
}

// The BNO080 supports six communication channels:
const  SHTP_CHAN_COMMAND: u8 = 0; /// the SHTP command channel
const  CHANNEL_EXECUTABLE: u8 = 1; /// executable channel
const  CHANNEL_HUB_CONTROL: u8 = 2; /// sensor hub control channel
const  CHANNEL_SENSOR_REPORTS: u8 = 3; /// input sensor reports (non-wake, not gyroRV)
//const  CHANNEL_WAKE_REPORTS: usize = 4; /// wake input sensor reports (for sensors configured as wake up sensors)
//const  CHANNEL_GYRO_ROTATION: usize = 5; ///  gyro rotation vector (gyroRV)


/// SHTP constants
const SENSORHUB_PROD_ID_REQ: u8 = 0xF9;
const SENSORHUB_PROD_ID_RESP: u8 =  0xF8;

const SHTP_REPORT_SET_FEATURE_COMMAND: u8 = 0xFD;

const SENSOR_REPORTID_ROTATION_VECTOR: u8 = 0x05;


/// requests
//const SENSORHUB_COMMAND_REQ:u8 =  0xF2;
const SENSORHUB_COMMAND_RESP:u8 = 0xF1;


/// executable/device channel responses
/// Figure 1-27: SHTP executable commands and response
// const EXECUTABLE_DEVICE_CMD_UNKNOWN: u8 =  0;
const EXECUTABLE_DEVICE_CMD_RESET: u8 =  1;
//const EXECUTABLE_DEVICE_CMD_ON: u8 =   2;
//const EXECUTABLE_DEVICE_CMD_SLEEP =  3;

/// Response to CMD_RESET
const EXECUTABLE_DEVICE_RESP_RESET_COMPLETE: u8 = 1;

/// Commands and subcommands
const SH2_INIT_UNSOLICITED: u8 = 0x80;
const SH2_CMD_INITIALIZE: u8 = 4;
//const SH2_INIT_SYSTEM: u8 = 1;
const SH2_STARTUP_INIT_UNSOLICITED:u8 = SH2_CMD_INITIALIZE | SH2_INIT_UNSOLICITED;

#[cfg(test)]
mod tests {
    use crate::interface::mock_i2c_port::FakeI2cPort;
    use super::BNO080;
    //use super::*;

    use crate::interface::I2cInterface;
    use crate::interface::i2c::DEFAULT_ADDRESS;



//    #[test]
//    fn test_receive_unsized_under() {
//        let mut mock_i2c_port = FakeI2cPort::new();
//
//        let packet: [u8; 3] = [0; 3];
//        mock_i2c_port.add_available_packet( &packet);
//
//        let mut shub = BNO080::new(mock_i2c_port);
//        let rc = shub.read_unsized_packet();
//        assert!(rc.is_err());
//    }

    // //TODO give access to sent packets for testing porpoises
    // #[test]
    // fn test_send_reset() {
    //     let mut mock_i2c_port = FakeI2cPort::new();
    //     let mut shub = Wrapper::new_with_interface(
    //         I2cInterface::new(mock_i2c_port, DEFAULT_ADDRESS));
    //     let rc = shub.soft_reset();
    //     let sent_pack = shub.sensor_interface.sent_packets.pop_front().unwrap();
    //     assert_eq!(sent_pack.len, 5);
    // }

    pub const MIDPACK: [u8; 52] = [
        0x34, 0x00, 0x02, 0x7B,
        0xF8, 0x00, 0x01, 0x02,
        0x96, 0xA4, 0x98, 0x00,
        0xE6, 0x00, 0x00, 0x00,
        0x04, 0x00, 0x00, 0x00,
        0xF8, 0x00, 0x04, 0x04,
        0x36, 0xA3, 0x98, 0x00,
        0x95, 0x01, 0x00, 0x00,
        0x02, 0x00, 0x00, 0x00,
        0xF8, 0x00, 0x04, 0x02,
        0xE3, 0xA2, 0x98, 0x00,
        0xD9, 0x01, 0x00, 0x00,
        0x07, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn test_receive_midpack() {
        let mut mock_i2c_port = FakeI2cPort::new();

        let packet = MIDPACK;
        mock_i2c_port.add_available_packet( &packet);

        let mut shub = BNO080::new_with_interface(
            I2cInterface::new(mock_i2c_port, DEFAULT_ADDRESS));
        let rc = shub.receive_packet();
        assert!(rc.is_ok());
    }


    #[test]
    fn test_handle_adv_message() {
        let mut mock_i2c_port = FakeI2cPort::new();

        //actual startup response packet
        let raw_packet = ADVERTISING_PACKET_FULL;
        mock_i2c_port.add_available_packet( &raw_packet);

        let mut shub = BNO080::new_with_interface(
            I2cInterface::new(mock_i2c_port, DEFAULT_ADDRESS));

        let msg_count = shub.handle_one_message();
        assert_eq!(msg_count, 1, "wrong msg_count");

    }

    // Actual advertising packet received from sensor:
    pub const ADVERTISING_PACKET_FULL: [u8; 276] = [
        0x14, 0x81, 0x00, 0x01,
        0x00, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x80, 0x06, 0x31, 0x2e, 0x30, 0x2e, 0x30, 0x00, 0x02, 0x02, 0x00, 0x01, 0x03, 0x02, 0xff, 0x7f, 0x04, 0x02, 0x00, 0x01, 0x05,
        0x02, 0xff, 0x7f, 0x08, 0x05, 0x53, 0x48, 0x54, 0x50, 0x00, 0x06, 0x01, 0x00, 0x09, 0x08, 0x63, 0x6f, 0x6e, 0x74, 0x72, 0x6f, 0x6c, 0x00, 0x01, 0x04, 0x01, 0x00, 0x00,
        0x00, 0x08, 0x0b, 0x65, 0x78, 0x65, 0x63, 0x75, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x00, 0x06, 0x01, 0x01, 0x09, 0x07, 0x64, 0x65, 0x76, 0x69, 0x63, 0x65, 0x00, 0x01, 0x04,
        0x02, 0x00, 0x00, 0x00, 0x08, 0x0a, 0x73, 0x65, 0x6e, 0x73, 0x6f, 0x72, 0x68, 0x75, 0x62, 0x00, 0x06, 0x01, 0x02, 0x09, 0x08, 0x63, 0x6f, 0x6e, 0x74, 0x72, 0x6f, 0x6c,
        0x00, 0x06, 0x01, 0x03, 0x09, 0x0c, 0x69, 0x6e, 0x70, 0x75, 0x74, 0x4e, 0x6f, 0x72, 0x6d, 0x61, 0x6c, 0x00, 0x07, 0x01, 0x04, 0x09, 0x0a, 0x69, 0x6e, 0x70, 0x75, 0x74,
        0x57, 0x61, 0x6b, 0x65, 0x00, 0x06, 0x01, 0x05, 0x09, 0x0c, 0x69, 0x6e, 0x70, 0x75, 0x74, 0x47, 0x79, 0x72, 0x6f, 0x52, 0x76, 0x00, 0x80, 0x06, 0x31, 0x2e, 0x31, 0x2e,
        0x30, 0x00, 0x81, 0x64, 0xf8, 0x10, 0xf5, 0x04, 0xf3, 0x10, 0xf1, 0x10, 0xfb, 0x05, 0xfa, 0x05, 0xfc, 0x11, 0xef, 0x02, 0x01, 0x0a, 0x02, 0x0a, 0x03, 0x0a, 0x04, 0x0a,
        0x05, 0x0e, 0x06, 0x0a, 0x07, 0x10, 0x08, 0x0c, 0x09, 0x0e, 0x0a, 0x08, 0x0b, 0x08, 0x0c, 0x06, 0x0d, 0x06, 0x0e, 0x06, 0x0f, 0x10, 0x10, 0x05, 0x11, 0x0c, 0x12, 0x06,
        0x13, 0x06, 0x14, 0x10, 0x15, 0x10, 0x16, 0x10, 0x17, 0x00, 0x18, 0x08, 0x19, 0x06, 0x1a, 0x00, 0x1b, 0x00, 0x1c, 0x06, 0x1d, 0x00, 0x1e, 0x10, 0x1f, 0x00, 0x20, 0x00,
        0x21, 0x00, 0x22, 0x00, 0x23, 0x00, 0x24, 0x00, 0x25, 0x00, 0x26, 0x00, 0x27, 0x00, 0x28, 0x0e, 0x29, 0x0c, 0x2a, 0x0e
    ];


}
