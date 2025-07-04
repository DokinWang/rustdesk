use lazy_static::lazy_static;
use serialport::SerialPort;
use std::fmt;
use std::io;
use std::sync::Mutex;

lazy_static! {
    pub static ref SERIAL: Mutex<Option<SerialWriter>> = Mutex::new(None);
}

pub struct SerialWriter {
    port: Box<dyn SerialPort>,
}

impl SerialWriter {
    pub fn auto_init() -> io::Result<()> {
        let mut guard = SERIAL.lock().unwrap();
        if guard.is_none() {
            let ports = serialport::available_ports()?;
            if let Some(port) = ports.first() {
                println!("[Auto-init serial] {}", port.port_name);
                *guard = Some(SerialWriter {
                    port: serialport::new(&port.port_name, 115200).open()?,
                });
            }
        }
        Ok(())
    }

    // 新增方法：处理格式化错误转换
    fn write_fmt_io(&mut self, args: fmt::Arguments) -> io::Result<()> {
        self.port.write_all(args.to_string().as_bytes())
    }
}

// 移除 fmt::Write 的实现，改用上面的新方法

// 公开的打印函数
pub fn serial_println(args: fmt::Arguments) -> io::Result<()> {
    let _ = SerialWriter::auto_init();
    if let Ok(mut guard) = SERIAL.lock() {
        if let Some(writer) = &mut *guard {
            writer.write_fmt_io(args)?;
            writer.port.write_all(b"\n")?;
        }
    }
    Ok(())
}