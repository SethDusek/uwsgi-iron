#![allow(unused_mut)]
#![feature(box_syntax)]

extern crate libloading;
extern crate iron;
extern crate hyper;
extern crate libc;

use libloading::os::unix::*;
use libc::c_void;
use std::collections::HashMap;
use iron::{Handler, status, Url, Headers};
use std::{io, mem};
use iron::request::Body;
use iron::response::{ResponseBody, WriteBody};
use hyper::http::h1::HttpReader;
use hyper::net::NetworkStream;
use iron::prelude::*;
use iron::method;
use std::io::prelude::*;
use std::str::FromStr;
use std::io::Cursor;
use hyper::buffer::BufReader;
use std::str;
use std::time::Duration;
use std::net::{IpAddr, SocketAddr};
use std::slice;

// global access to the function entry point (could become a vector to support multple apps)
type RustFn = Symbol<extern fn(HashMap<&str, &str>) -> (String, Vec<(String, String)>, Vec<u8>)>;
static mut app: Option<RustFn> = None;
static mut handler: Option<RustFn> = None;

// C functions used by Rust
fn handlee(_: &mut Request) -> IronResult<Response> {
    Ok(Response::with((status::Ok, "Hello World!")))
}
extern {
	fn uwsgi_response_prepare_headers(wsgi_req: *mut c_void, buf: *mut u8, buf_len: u16) -> i32;
	fn uwsgi_response_add_header(wsgi_req: *mut c_void, key: *mut u8, key_len: u16, val: *mut u8, val_len: u16) -> i32;
	fn uwsgi_response_write_body_do(wsgi_req: *mut c_void, buf: *mut u8, buf_len: u64) -> i32;

	fn uwsgi_rust_build_environ(wsgi_req: *mut c_void, environ: &HashMap<&str, &str>) -> i32;
    fn uwsgi_request_body_read(wsgi_req: *mut c_void, hint: isize, request_len: *const usize) -> *mut libc::c_char;
}

// load the function entry point
#[no_mangle]
pub extern fn rust_load_fn(name: *mut u8, name_len: u16) -> i32 {
	let lib = Library::this(); 
	let fn_name_slice = unsafe { slice::from_raw_parts(name, name_len as usize) };
	unsafe {
        app = match lib.get(fn_name_slice) {
                Ok(symbol) => Some(symbol),
                Err(e) => { println!("[rust] {}", e); return -1 }
                }
    };

	0
}

// populate the environ HashMap with CGI vars
#[no_mangle]
pub extern fn rust_add_environ(environ: *mut HashMap<&str, &str>, key: *mut u8, key_len: u16, val: *mut u8, val_len: u16) -> i32 {
	let k = unsafe { slice::from_raw_parts(key, key_len as usize) };
	let sk = match str::from_utf8(k) {
		Ok(s) => s,
		Err(e) => { println!("[rust] {}", e); return -1 },
	};

	let v = unsafe { slice::from_raw_parts(val, val_len as usize) };
	let sv = match str::from_utf8(v) {
		Ok(s) => s,
		Err(e) => { println!("[rust] {}", e); return -1 },
	};

	unsafe {
		(*environ).insert(sk, sv);
	}

	0
}

fn translate_to_request<'a>(environ: &HashMap<&str, &str>, body: &'a mut BufReader<&mut NetworkStream>, len: u64) -> Request<'a> {
    let url = Url::parse(&("http://".to_owned() + environ.get("HTTP_HOST").unwrap() + environ.get("REQUEST_URI").unwrap())).unwrap();
    let mut headers = Headers::new();
    for (field, value) in environ.iter() {
        headers.set_raw(field.to_string(), vec![value.as_bytes().to_owned()]);
    }

    Request {
        url: url,
        remote_addr: SocketAddr::new(IpAddr::from_str("127.0.0.1").unwrap(), 34612), //not yet implemented
        /*local_addr: {
            let splitted: Vec<&str> = environ.get("HTTP_HOST").unwrap().split(":").collect();
            SocketAddr::new(IpAddr::from_str(splitted[0]).unwrap(), u16::from_str(splitted[1]).unwrap())
        },*/
        local_addr: SocketAddr::new(IpAddr::from_str("127.0.0.1").unwrap(), 20), //not yet implemented either
        headers: headers,
        body: Body::new(HttpReader::SizedReader(body, len)),
        method: method::Method::from_str(environ.get("REQUEST_METHOD").unwrap()).unwrap(),
        extensions: iron::typemap::TypeMap::new()
    }
}

fn translate_from_response(resp: Response) -> (String, Vec<(String, String)>, Vec<u8>) {
    let mut body = Vec::new();
    let mut status_code = String::new();
    let mut headers = Vec::new();
    {
        let mut resp_body = ResponseBody::new(&mut body);
        if let Some(mut body) = resp.body {
            body.write_body(&mut resp_body);
        }
    }
    if let Some(status) = resp.status {
        if let Some(canonical) = status.canonical_reason() {
            status_code.push_str(canonical);
        }
    }
    else {
        status_code.push_str("200 OK");
    }

    for header in resp.headers.iter() {
        headers.push((header.name().to_owned(), header.value_string()));
    }


    (status_code, headers, body)
}


// run the entry point and send its response to the client
#[no_mangle]
pub extern fn rust_request_handler(wsgi_req: *mut c_void) -> i32 {
	let mut environ = HashMap::new();
    let mut len = 0usize;
    let body = unsafe {
        let ptr = uwsgi_request_body_read(wsgi_req, 8192, &len as *const usize) as *mut u8;
        slice::from_raw_parts_mut(ptr, len).to_owned()
    };
    let mut stream = Curser(Cursor::new(body));
    let mut body = BufReader::new(&mut stream as &mut NetworkStream);
	unsafe {
		if uwsgi_rust_build_environ(wsgi_req, &environ) != 0 {
			return -1;
		}
	}
    let mut request = translate_to_request(&environ, unsafe { &mut *(&mut body as *mut _) }, len as u64);    
	let entry_point = unsafe {
		    match app {
			    None => return -1,
			    Some(ref f) => f,
		}
	};

	let (status, headers, body) = entry_point(environ);

	unsafe {
		let ret = uwsgi_response_prepare_headers(wsgi_req, status.as_ptr() as *mut u8, status.into_bytes().len() as u16);
		if ret != 0 {
			return ret;
		}
	}

	for header in headers {
		unsafe {
			let ret = uwsgi_response_add_header(wsgi_req, header.0.as_ptr() as *mut u8, header.0.into_bytes().len() as u16,
				header.1.as_ptr() as *mut u8, header.1.into_bytes().len() as u16);
			if ret != 0 {
				return ret;
			}
		}
	}

	/*for chunk in body {
		unsafe {
			let ret = uwsgi_response_write_body_do(wsgi_req, chunk.as_ptr() as *mut u8, chunk.len() as u64);
			if ret != 0 {
				return ret;
			}
		}
	}*/
    unsafe {
        let ret = uwsgi_response_write_body_do(wsgi_req, body.as_ptr() as *mut u8, body.len() as u64);
        if ret != 0 {
            return ret;
        }
        }

	0
}

struct Curser(Cursor<Vec<u8>>); //Wrapper around Cursor because rust doesn't let us implement traits on cursor otherwise

unsafe impl Send for Curser {}

impl Read for Curser {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Curser {
    #[inline(always)]
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        self.0.write(buf)
    }
    #[inline(always)]
    fn flush(&mut self) -> Result<(), io::Error> {
        self.0.flush()
    }
}

impl NetworkStream for Curser {
    fn peer_addr(&mut self) -> Result<SocketAddr, io::Error> { Ok(SocketAddr::from_str("http://localhost:8080").unwrap()) }
    fn set_read_timeout(&self, _: Option<Duration>) -> Result<(), std::io::Error> { Ok(()) }
    fn set_write_timeout(&self, _: Option<Duration>) -> Result<(), std::io::Error> { Ok(()) }
}
