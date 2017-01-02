use std::ops::Deref;
use std::ptr;
use std::ffi::{CString, CStr};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::path::Path;
use std::marker::PhantomData;
use std::io::{self, Write};

use ::{AltoError, AltoResult};
use sys;
use al::*;
use ext;


/// Attributes that may be supplied during context creation.
pub struct ContextAttrs {
	/// Output sampling rate of the audio.
	pub frequency: Option<sys::ALCint>,
	/// Refresh rate of the internal mixer, in Hz.
	pub refresh: Option<sys::ALCint>,
	/// Hint for number of mono sources that will be created.
	pub mono_sources: Option<sys::ALCint>,
	/// Hint for number of stereo sources that will be created.
	pub stereo_sources: Option<sys::ALCint>,
	/// Whether HRTF is desired.
	pub soft_hrtf: Option<bool>,
	/// The ID of the HRTF specifier to be used.
	/// This should be the index of a specifier as retrieved from [`enumerate_soft_hrtfs`](trait.DeviceTrait.html#tymethod.enumerate_soft_hrtfs).
	pub soft_hrtf_id: Option<sys::ALCint>,
}


/// Attributes that may be supplied during context creation from a loopback device.
pub struct LoopbackAttrs {
	/// Hint for number of mono sources that will be created.
	pub mono_sources: Option<sys::ALCint>,
	/// Hint for number of stereo sources that will be created.
	pub stereo_sources: Option<sys::ALCint>,
	/// Whether HRTF is desired.
	pub soft_hrtf: Option<bool>,
	/// The ID of the HRTF specifier to be used.
	pub soft_hrtf_id: Option<sys::ALCint>,
}


/// Channel format for a loopback context.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LoopbackFormatChannels {
	Mono,
	Stereo,
	Quad,
	Mc51,
	Mc61,
	Mc71,
}


/// Sample format for a loopback context.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LoopbackFormatType {
	U8,
	I16,
	F32,
}


/// The current HRTF mode of a device.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SoftHrtfStatus {
	Disabled,
	Enabled,
	Denied,
	Required,
	HeadphonesDetected,
	UnsupportedFormat,
	Unknown(sys::ALCint),
}




rental!{
	mod rent {
		pub rental AlApi<'rental>(Box<::sys::AlApi>, ::ext::AlcNullCache<'rental>);
	}
}

#[doc(hidden)]
pub use self::rent::AlApi;


/// This struct is the entry point of the API. Instantiating it will load an OpenAL implementation
/// dynamically. From here, available devices can be queried and opened.
pub struct Alto {
	api: AlApi<'static>,
	ctx_lock: Mutex<()>,
}


/// Common capabilities expoed by both real and loopback devices.
pub trait DeviceTrait {
	/// Alto instance from which this device was opened.
	fn alto(&self) -> &Alto;
	/// Specifier string used to open this device.
	fn specifier(&self) -> &CStr;
	/// Raw handle as exposed by OpenAL.
	fn raw_device(&self) -> *mut sys::ALCdevice;
	/// Query the presence of an ALC extension.
	fn is_extension_present(&self, ext::Alc) -> bool;
	/// Polls the connection state.
	/// If this ever returns false, then the device must be closed and reopened; it will not become true again.
	fn connected(&self) -> AltoResult<bool>;
	/// Enumerate the supported HRTF functions.
	fn enumerate_soft_hrtfs(&self) -> AltoResult<Vec<CString>>;
	/// Current HRTF mode.
	fn soft_hrtf_status(&self) -> AltoResult<SoftHrtfStatus>;
}


/// An audio device as exposed by the OpenAL implementation.
/// This will typically be a device endpoint as reported by the operating system.
pub struct Device<'a> {
	alto: &'a Alto,
	spec: CString,
	dev: *mut sys::ALCdevice,
	exts: ext::AlcCache<'a>,
	pause_rc: Arc<AtomicUsize>,
}


/// An RAII guard that keeps a device paused.
/// When this lock is dropped, the device will resume playback.
pub struct SoftPauseLock<'a: 'd, 'd>(&'d Device<'a>);


/// A sample frame that is supported as a loopback device output format.
pub unsafe trait LoopbackFrame: SampleFrame {
	fn channels(&ext::ALC_SOFT_loopback) -> AltoResult<sys::ALint>;
	fn sample_ty(&ext::ALC_SOFT_loopback) -> AltoResult<sys::ALint>;
}


/// A loopback device as provided by the `ALC_SOFT_loopback` extension.
pub struct LoopbackDevice<'a, F: LoopbackFrame> {
	alto: &'a Alto,
	spec: CString,
	dev: *mut sys::ALCdevice,
	exts: ext::AlcCache<'a>,
	marker: PhantomData<F>,
}


/// A capture device from which audio data can be sampled.
/// This is tyically an audio input as reported by the operating system.
pub struct CaptureDevice<'a> {
	alto: &'a Alto,
	spec: CString,
	dev: *mut sys::ALCdevice,
}


impl Alto {
	/// Load the default OpenAL implementation for the platform.
	/// This will prefer OpenAL-Soft if it is present, otherwise it will search for a generic implementation.
	pub fn load_default() -> AltoResult<Alto> {
		let api = Box::new(sys::AlApi::load_default()?);
		Ok(Alto{
			api: AlApi::new(api, |a| unsafe { ext::AlcNullCache::new(a, ptr::null_mut()) }),
			ctx_lock: Mutex::new(()),
		}).and_then(|a| a.check_version())
	}


	/// Loads a specific OpenAL implementation from a specififed path.
	pub fn load<P: AsRef<Path>>(path: P) -> AltoResult<Alto> {
		let api = Box::new(sys::AlApi::load(path)?);
		Ok(Alto{
			api: AlApi::new(api, |a| unsafe { ext::AlcNullCache::new(a, ptr::null_mut()) }),
			ctx_lock: Mutex::new(()),
		}).and_then(|a| a.check_version())
	}


	fn check_version(self) -> AltoResult<Alto> {
		let mut major = 0;
		unsafe { self.api.owner().alcGetIntegerv()(ptr::null_mut(), sys::ALC_MAJOR_VERSION, 1, &mut major); }
		let mut minor = 0;
		unsafe { self.api.owner().alcGetIntegerv()(ptr::null_mut(), sys::ALC_MINOR_VERSION, 1, &mut minor); }

		if major == 1 && minor >= 1 {
			Ok(self)
		} else {
			Err(AltoError::AlcUnsupportedVersion)
		}
	}


	/// Get the specifier of the default output device.
	pub fn default_output(&self) -> AltoResult<CString> {
		self.api.rent(|exts| {
			let spec = if let Ok(ea) = exts.ALC_ENUMERATE_ALL_EXT() {
				unsafe { CStr::from_ptr(self.api.owner().alcGetString()(ptr::null_mut(), ea.ALC_DEFAULT_ALL_DEVICES_SPECIFIER?)) }
			} else {
				unsafe { CStr::from_ptr(self.api.owner().alcGetString()(ptr::null_mut(), sys::ALC_DEFAULT_DEVICE_SPECIFIER)) }
			};
			self.get_error(ptr::null_mut()).map(|_| spec.to_owned())
		})
	}


	/// Get the specifier of the default capture device.
	pub fn default_capture(&self) -> AltoResult<CString> {
		let spec = unsafe { CStr::from_ptr(self.api.owner().alcGetString()(ptr::null_mut(), sys::ALC_CAPTURE_DEFAULT_DEVICE_SPECIFIER)) };
		self.get_error(ptr::null_mut()).map(|_| spec.to_owned())
	}


	/// Enumerate available audio outputs detected by the implementation.
	pub fn enumerate_outputs(&self) -> AltoResult<Vec<CString>> {
		self.api.rent(|exts| {
			let spec = if let Ok(ea) = exts.ALC_ENUMERATE_ALL_EXT() {
				unsafe { self.api.owner().alcGetString()(ptr::null_mut(), ea.ALC_ALL_DEVICES_SPECIFIER?) }
			} else {
				unsafe { self.api.owner().alcGetString()(ptr::null_mut(), sys::ALC_DEVICE_SPECIFIER) }
			};
			self.get_error(ptr::null_mut()).and_then(|_| Alto::parse_enum_spec(spec as *const u8))
		})
	}


	/// Enumerate available audio inputs detected by the implementation.
	pub fn enumerate_captures(&self) -> AltoResult<Vec<CString>> {
		let spec = unsafe { self.api.owner().alcGetString()(ptr::null_mut(), sys::ALC_CAPTURE_DEVICE_SPECIFIER) };
		self.get_error(ptr::null_mut()).and_then(|_| Alto::parse_enum_spec(spec as *const u8))
	}


	fn parse_enum_spec(spec: *const u8) -> AltoResult<Vec<CString>> {
		let mut specs = Vec::with_capacity(0);

		let mut i = 0;
		loop {
			if unsafe { ptr::read(spec.offset(i)) == 0 && ptr::read(spec.offset(i + 1)) == 0 } {
				break;
			}

			i += 1;
		}

		specs.extend(unsafe { ::std::slice::from_raw_parts(spec as *const u8, i as usize) }.split(|c| *c == 0).map(|d| CString::new(d).unwrap()));

		Ok(specs)
	}


	/// Open an audio device from a device specifier, or default if `None`.
	pub fn open(&self, spec: Option<&CStr>) -> AltoResult<Device> {
		let spec = if let Some(spec) = spec {
			spec.to_owned()
		} else {
			self.default_output()?
		};

		let dev = unsafe { self.api.owner().alcOpenDevice()(spec.as_ptr()) };
		self.get_error(ptr::null_mut())?;

		if dev == ptr::null_mut() {
			Err(AltoError::AlcInvalidDevice)
		} else {
			Ok(Device{
				alto: self,
				spec: spec,
				dev: dev,
				exts: unsafe { ext::AlcCache::new(self.api.owner(), dev) },
				pause_rc: Arc::new(AtomicUsize::new(0))
			})
		}
	}


	/// Open a loopback device from a device specifier, or default if `None`.
	pub fn open_loopback<F: LoopbackFrame>(&self, spec: Option<&CStr>) -> AltoResult<LoopbackDevice<F>> {
		self.api.rent(|exts| {
			let sl = exts.ALC_SOFT_loopback()?;

			let spec = if let Some(spec) = spec {
				spec.to_owned()
			} else {
				self.default_output()?
			};

			let dev = unsafe { sl.alcLoopbackOpenDeviceSOFT?(spec.as_ptr()) };
			self.get_error(ptr::null_mut())?;

			if dev == ptr::null_mut() {
				Err(AltoError::AlcInvalidDevice)
			} else {
				Ok(LoopbackDevice{
					alto: self,
					spec: spec,
					dev: dev,
					exts: unsafe { ext::AlcCache::new(self.api.owner(), dev) },
					marker: PhantomData
				})
			}
		})
	}


	/// Open a capture device from a device specifier, or default if `None`.
	pub fn open_capture(&self, spec: Option<&CStr>, freq: sys::ALCuint, format: StandardFormat, size: sys::ALCsizei) -> AltoResult<CaptureDevice> {
		let spec = if let Some(spec) = spec {
			spec.to_owned()
		} else {
			self.default_output()?
		};

		let dev = unsafe { self.api.owner().alcCaptureOpenDevice()(spec.as_ptr(), freq, format.into_raw(), size) };
		self.get_error(ptr::null_mut())?;

		if dev == ptr::null_mut() {
			Err(AltoError::AlcInvalidDevice)
		} else {
			Ok(CaptureDevice{alto: self, spec: spec, dev: dev})
		}
	}


	#[doc(hidden)]
	pub fn get_error(&self, dev: *mut sys::ALCdevice) -> AltoResult<()> {
		match unsafe { self.api.owner().alcGetError()(dev)} {
			sys::ALC_NO_ERROR => Ok(()),
			e => Err(AltoError::from_alc(e)),
		}
	}
}


impl<'a> PartialEq for (DeviceTrait + 'a) {
	fn eq(&self, other: &(DeviceTrait + 'a)) -> bool {
		self.raw_device() == other.raw_device()
	}
}
impl Eq for DeviceTrait { }


impl<'a> Device<'a> {
	fn make_attrs_vec(&self, attrs: Option<ContextAttrs>) -> AltoResult<Vec<sys::ALCint>> {
		let mut attrs_vec = Vec::with_capacity(13);
		if let Some(attrs) = attrs {
			if let Some(freq) = attrs.frequency {
				attrs_vec.extend(&[sys::ALC_FREQUENCY, freq]);
			}
			if let Some(refresh) = attrs.refresh {
				attrs_vec.extend(&[sys::ALC_REFRESH, refresh]);
			}
			if let Some(mono) = attrs.mono_sources {
				attrs_vec.extend(&[sys::ALC_MONO_SOURCES, mono]);
			}
			if let Some(stereo) = attrs.stereo_sources {
				attrs_vec.extend(&[sys::ALC_STEREO_SOURCES, stereo]);
			}

			if let Ok(ash) = self.exts.ALC_SOFT_HRTF() {
				if let Some(hrtf) = attrs.soft_hrtf {
					attrs_vec.extend(&[ash.ALC_HRTF_SOFT?, if hrtf { sys::ALC_TRUE } else { sys::ALC_FALSE } as sys::ALCint]);
				}
				if let Some(hrtf_id) = attrs.soft_hrtf_id {
					attrs_vec.extend(&[ash.ALC_HRTF_ID_SOFT?, hrtf_id]);
				}
			}

			attrs_vec.push(0);
		};
		Ok(attrs_vec)
	}


	/// Create a new context from this device.
	pub fn new_context(&self, attrs: Option<ContextAttrs>) -> AltoResult<Context> {
		let attrs_vec = self.make_attrs_vec(attrs);

		let ctx = unsafe { self.alto.api.owner().alcCreateContext()(self.dev, attrs_vec.map(|a| a.as_slice().as_ptr()).unwrap_or(ptr::null())) };
		self.alto.get_error(self.dev).map(|_| unsafe { Context::new(self, &self.alto.api, &self.alto.ctx_lock, ctx) })
	}


	/// Pause output of this device and return a lock.
	/// Output will resume when this lock is dropped.
	pub fn soft_pause<'d>(&'d self) -> AltoResult<SoftPauseLock<'a, 'd>> {
		SoftPauseLock::new(self)
	}


	/// Attempt to reset the device with new attributes.
	/// Requires the `ALC_SOFT_HRTF`.
	pub fn soft_reset(&self, attrs: Option<ContextAttrs>) -> AltoResult<()> {
		let ards = self.exts.ALC_SOFT_HRTF()?.alcResetDeviceSOFT?;
		let attrs_vec = self.make_attrs_vec(attrs);
		unsafe { ards(self.dev, attrs_vec.map(|a| a.as_slice().as_ptr()).unwrap_or(ptr::null())) };
		self.alto.get_error(self.dev)
	}
}


impl<'a> DeviceTrait for Device<'a> {
	#[inline]
	fn alto(&self) -> &Alto { &self.alto }
	#[inline]
	fn specifier(&self) -> &CStr { &self.spec }
	#[inline]
	fn raw_device(&self) -> *mut sys::ALCdevice { self.dev }


	fn is_extension_present(&self, ext: ext::Alc) -> bool {
		match ext {
			ext::Alc::Dedicated => self.exts.ALC_EXT_DEDICATED().is_ok(),
			ext::Alc::Disconnect => self.exts.ALC_EXT_DISCONNECT().is_ok(),
			ext::Alc::Efx => self.exts.ALC_EXT_EFX().is_ok(),
			ext::Alc::SoftHrtf => self.exts.ALC_SOFT_HRTF().is_ok(),
			ext::Alc::SoftPauseDevice => self.exts.ALC_SOFT_pause_device().is_ok(),
		}
	}


	fn connected(&self) -> AltoResult<bool> {
		let mut connected = 0;
		unsafe { self.alto.api.owner().alcGetIntegerv()(self.dev, self.exts.ALC_EXT_DISCONNECT()?.ALC_CONNECTED?, 1, &mut connected); }
		self.alto.get_error(self.dev).map(|_| connected == sys::ALC_TRUE as sys::ALCint)
	}


	fn enumerate_soft_hrtfs(&self) -> AltoResult<Vec<CString>> {
		let ash = self.exts.ALC_SOFT_HRTF()?;

		let mut num = 0;
		unsafe { self.alto.api.owner().alcGetIntegerv()(self.dev, ash.ALC_NUM_HRTF_SPECIFIERS_SOFT?, 1, &mut num); }
		self.alto.get_error(self.dev)?;

		let mut spec_vec = Vec::new();
		for i in 0 .. num {
			unsafe {
				let spec = ash.alcGetStringiSOFT?(self.dev, ash.ALC_HRTF_SPECIFIER_SOFT?, i) as *mut _;
				spec_vec.push(self.alto.get_error(self.dev).map(|_| CString::from_raw(spec))?);
			}
		}
		Ok(spec_vec)
	}


	fn soft_hrtf_status(&self) -> AltoResult<SoftHrtfStatus> {
		let ash = self.exts.ALC_SOFT_HRTF()?;

		let mut status = 0;
		unsafe { self.alto.api.owner().alcGetIntegerv()(self.dev, ash.ALC_HRTF_STATUS_SOFT?, 1, &mut status); }
		self.alto.get_error(self.dev).and_then(|_| match status {
			s if s == ash.ALC_HRTF_DISABLED_SOFT? => Ok(SoftHrtfStatus::Disabled),
			s if s == ash.ALC_HRTF_ENABLED_SOFT? => Ok(SoftHrtfStatus::Enabled),
			s if s == ash.ALC_HRTF_DENIED_SOFT? => Ok(SoftHrtfStatus::Denied),
			s if s == ash.ALC_HRTF_REQUIRED_SOFT? => Ok(SoftHrtfStatus::Required),
			s if s == ash.ALC_HRTF_HEADPHONES_DETECTED_SOFT? => Ok(SoftHrtfStatus::HeadphonesDetected),
			s if s == ash.ALC_HRTF_UNSUPPORTED_FORMAT_SOFT? => Ok(SoftHrtfStatus::UnsupportedFormat),
			s => Ok(SoftHrtfStatus::Unknown(s)),
		})
	}
}


impl<'a> PartialEq for Device<'a> {
	fn eq(&self, other: &Device<'a>) -> bool {
		self.dev == other.dev
	}
}
impl<'a> Eq for Device<'a> { }


impl<'a> Drop for Device<'a> {
	fn drop(&mut self) {
		unsafe { self.alto.api.owner().alcCloseDevice()(self.dev); }
		if let Err(_) = self.alto.get_error(self.dev) {
			let _ = writeln!(io::stderr(), "ALTO ERROR: `alcCloseDevice` failed in Device drop");
		}
	}
}


unsafe impl<'a> Send for Device<'a> { }
unsafe impl<'a> Sync for Device<'a> { }


impl<'a: 'd, 'd> SoftPauseLock<'a, 'd> {
	fn new(dev: &'d Device<'a>) -> AltoResult<SoftPauseLock<'a, 'd>> {
		let adps = dev.exts.ALC_SOFT_pause_device()?.alcDevicePauseSOFT?;

		let old = dev.pause_rc.fetch_add(1, Ordering::SeqCst);
		if old == 0 {
			unsafe { adps(dev.dev) }
			if let Err(e) = dev.alto.get_error(dev.dev) {
				dev.pause_rc.fetch_sub(1, Ordering::SeqCst);
				return Err(e);
			}
		}

		Ok(SoftPauseLock(dev))
	}
}


impl<'a: 'd, 'd> Deref for SoftPauseLock<'a, 'd> {
	type Target = Device<'a>;

	fn deref(&self) -> &Device<'a> { self.0 }
}


impl<'a: 'd, 'd> Drop for SoftPauseLock<'a, 'd> {
	fn drop(&mut self) {
		let old = self.0.pause_rc.fetch_sub(1, Ordering::SeqCst);
		if old == 1 {
			unsafe { self.0.exts.ALC_SOFT_pause_device().unwrap().alcDeviceResumeSOFT.unwrap()(self.0.dev); }
			if let Err(_) = self.0.alto.get_error(self.0.dev) {
				let _ = writeln!(io::stderr(), "ALTO ERROR: `alcDeviceResumeSOFT` failed in SoftPauseLock drop");
			}
		}
	}
}


impl<'a, F: LoopbackFrame> LoopbackDevice<'a, F> {
	fn make_attrs_vec(&self, freq: sys::ALCint, attrs: Option<LoopbackAttrs>) -> AltoResult<Vec<sys::ALCint>> {
		self.alto.api.rent(move|exts| {
			let asl = exts.ALC_SOFT_loopback()?;

			let mut attrs_vec = Vec::with_capacity(15);
			attrs_vec.extend(&[sys::ALC_FREQUENCY, freq]);
			attrs_vec.extend(&[asl.ALC_FORMAT_CHANNELS_SOFT?, F::channels(&asl)?]);
			attrs_vec.extend(&[asl.ALC_FORMAT_TYPE_SOFT?, F::sample_ty(&asl)?]);
			if let Some(attrs) = attrs {
				if let Some(mono) = attrs.mono_sources {
					attrs_vec.extend(&[sys::ALC_MONO_SOURCES, mono]);
				}
				if let Some(stereo) = attrs.stereo_sources {
					attrs_vec.extend(&[sys::ALC_STEREO_SOURCES, stereo]);
				}

				if let Ok(ash) = self.exts.ALC_SOFT_HRTF() {
					if let Some(hrtf) = attrs.soft_hrtf {
						attrs_vec.extend(&[ash.ALC_HRTF_SOFT?, if hrtf { sys::ALC_TRUE } else { sys::ALC_FALSE } as sys::ALCint]);
					}
					if let Some(hrtf_id) = attrs.soft_hrtf_id {
						attrs_vec.extend(&[ash.ALC_HRTF_ID_SOFT?, hrtf_id]);
					}
				}
			}
			attrs_vec.push(0);
			Ok(attrs_vec)
		})
	}


	/// Create a new context from this device.
	pub fn new_context(&self, freq: sys::ALCint, attrs: Option<LoopbackAttrs>) -> AltoResult<Context> {
		let attrs_vec = self.make_attrs_vec(freq, attrs)?;
		let ctx = unsafe { self.alto.api.owner().alcCreateContext()(self.dev, attrs_vec.as_slice().as_ptr()) };
		self.alto.get_error(self.dev).map(|_| unsafe { Context::new(self, &self.alto.api, &self.alto.ctx_lock, ctx) })
	}


	/// Attempt to reset the device with new attributes.
	/// Requires the `ALC_SOFT_HRTF`.
	pub fn soft_reset(&self, freq: sys::ALCint, attrs: Option<LoopbackAttrs>) -> AltoResult<()> {
		let ards = self.exts.ALC_SOFT_HRTF()?.alcResetDeviceSOFT?;

		let attrs_vec = self.make_attrs_vec(freq, attrs);
		unsafe { ards(self.dev, attrs_vec.map(|a| a.as_slice().as_ptr()).unwrap_or(ptr::null())) };
		self.alto.get_error(self.dev)
	}
}


impl<'a, F: LoopbackFrame> DeviceTrait for LoopbackDevice<'a, F> {
	#[inline]
	fn alto(&self) -> &Alto { &self.alto }
	#[inline]
	fn specifier(&self) -> &CStr { &self.spec }
	#[inline]
	fn raw_device(&self) -> *mut sys::ALCdevice { self.dev }
	#[inline]
	fn connected(&self) -> AltoResult<bool> { Ok(true) }


	fn is_extension_present(&self, ext: ext::Alc) -> bool {
		match ext {
			ext::Alc::Dedicated => self.exts.ALC_EXT_DEDICATED().is_ok(),
			ext::Alc::Disconnect => self.exts.ALC_EXT_DISCONNECT().is_ok(),
			ext::Alc::Efx => self.exts.ALC_EXT_EFX().is_ok(),
			ext::Alc::SoftHrtf => self.exts.ALC_SOFT_HRTF().is_ok(),
			ext::Alc::SoftPauseDevice => self.exts.ALC_SOFT_pause_device().is_ok(),
		}
	}


	fn enumerate_soft_hrtfs(&self) -> AltoResult<Vec<CString>> {
		let ash = self.exts.ALC_SOFT_HRTF()?;

		let mut num = 0;
		unsafe { self.alto.api.owner().alcGetIntegerv()(self.dev, ash.ALC_NUM_HRTF_SPECIFIERS_SOFT?, 1, &mut num); }
		self.alto.get_error(self.dev)?;

		let mut spec_vec = Vec::new();
		for i in 0 .. num {
			unsafe {
				let spec = ash.alcGetStringiSOFT?(self.dev, ash.ALC_HRTF_SPECIFIER_SOFT?, i) as *mut _;
				spec_vec.push(self.alto.get_error(self.dev).map(|_| CString::from_raw(spec))?);
			}
		}
		Ok(spec_vec)
	}


	fn soft_hrtf_status(&self) -> AltoResult<SoftHrtfStatus> {
		let ash = self.exts.ALC_SOFT_HRTF()?;

		let mut status = 0;
		unsafe { self.alto.api.owner().alcGetIntegerv()(self.dev, ash.ALC_HRTF_STATUS_SOFT?, 1, &mut status); }
		self.alto.get_error(self.dev).and_then(|_| match status {
			s if s == ash.ALC_HRTF_DISABLED_SOFT? => Ok(SoftHrtfStatus::Disabled),
			s if s == ash.ALC_HRTF_ENABLED_SOFT? => Ok(SoftHrtfStatus::Enabled),
			s if s == ash.ALC_HRTF_DENIED_SOFT? => Ok(SoftHrtfStatus::Denied),
			s if s == ash.ALC_HRTF_REQUIRED_SOFT? => Ok(SoftHrtfStatus::Required),
			s if s == ash.ALC_HRTF_HEADPHONES_DETECTED_SOFT? => Ok(SoftHrtfStatus::HeadphonesDetected),
			s if s == ash.ALC_HRTF_UNSUPPORTED_FORMAT_SOFT? => Ok(SoftHrtfStatus::UnsupportedFormat),
			s => Ok(SoftHrtfStatus::Unknown(s)),
		})
	}
}


impl<'a, F: LoopbackFrame> PartialEq for LoopbackDevice<'a, F> {
	fn eq(&self, other: &LoopbackDevice<'a, F>) -> bool {
		self.dev == other.dev
	}
}
impl<'a, F: LoopbackFrame> Eq for LoopbackDevice<'a, F> { }


impl<'a, F: LoopbackFrame> Drop for LoopbackDevice<'a, F> {
	fn drop(&mut self) {
		unsafe { self.alto.api.owner().alcCloseDevice()(self.dev); }
		if let Err(_) = self.alto.get_error(self.dev) {
			let _ = writeln!(io::stderr(), "ALTO ERROR: `alcCloseDevice` failed in LoopbackDevice drop");
		}
	}
}


unsafe impl<'a, F: LoopbackFrame> Send for LoopbackDevice<'a, F> { }
unsafe impl<'a, F: LoopbackFrame> Sync for LoopbackDevice<'a, F> { }


impl<'a> CaptureDevice<'a> {
	/// Alto struct from which this device was opened.
	#[inline]
	pub fn alto(&self) -> &Alto { &self.alto }
	/// Specifier used to open this device.
	#[inline]
	pub fn specifier(&self) -> &CStr { &self.spec }
	/// Raw device handle as reported by OpenAL.
	#[inline]
	pub fn raw_device(&self) -> *mut sys::ALCdevice { self.dev }
}


impl<'a> PartialEq for CaptureDevice<'a> {
	fn eq(&self, other: &CaptureDevice<'a>) -> bool {
		self.dev == other.dev
	}
}
impl<'a> Eq for CaptureDevice<'a> { }


unsafe impl<'a> Send for CaptureDevice<'a> { }
