//! Declarations of the C APIs chatty uses on macOS: CoreFoundation,
//! CoreGraphics (drawing, event taps, synthesized keys), CoreText, ImageIO,
//! Accessibility, IOKit HID and libdispatch. Only what is called is here.

#![allow(non_upper_case_globals, non_snake_case, dead_code, clippy::upper_case_acronyms)]

use std::ffi::{c_char, c_void};

// AppKit and QuartzCore are only used through the Objective-C runtime, so
// nothing would make the linker load them; NSFont, NSImage, NSApplication,
// CATransaction ... would not exist.
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}
#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {}

pub type CFTypeRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFAllocatorRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFSetRef = *const c_void;
pub type CFURLRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CFAttributedStringRef = *const c_void;
pub type CFMutableAttributedStringRef = *mut c_void;
pub type CFRunLoopRef = *mut c_void;
pub type CFRunLoopSourceRef = *mut c_void;
pub type CFMachPortRef = *mut c_void;
pub type CFIndex = isize;
pub type CFTypeID = usize;
pub type Boolean = u8;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CFRange {
    pub location: CFIndex,
    pub length: CFIndex,
}

#[repr(C)]
pub struct CFDictionaryKeyCallBacks {
    _private: [u8; 0],
}
#[repr(C)]
pub struct CFDictionaryValueCallBacks {
    _private: [u8; 0],
}

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;
pub const kCFNumberSInt32Type: CFIndex = 3;
pub const kCFNumberFloat64Type: CFIndex = 6;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub static kCFRunLoopCommonModes: CFStringRef;
    pub static kCFRunLoopDefaultMode: CFStringRef;
    pub static kCFBooleanTrue: CFTypeRef;
    pub static kCFBooleanFalse: CFTypeRef;
    pub static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
    pub static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;

    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    pub fn CFStringGetTypeID() -> CFTypeID;
    pub fn CFBooleanGetTypeID() -> CFTypeID;
    pub fn CFNumberGetTypeID() -> CFTypeID;
    pub fn CFArrayGetTypeID() -> CFTypeID;
    pub fn CFBooleanGetValue(b: CFTypeRef) -> Boolean;

    pub fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        len: CFIndex,
        encoding: u32,
        external: Boolean,
    ) -> CFStringRef;
    pub fn CFStringGetLength(s: CFStringRef) -> CFIndex;
    pub fn CFStringGetMaximumSizeForEncoding(len: CFIndex, encoding: u32) -> CFIndex;
    pub fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, encoding: u32) -> Boolean;

    pub fn CFNumberCreate(alloc: CFAllocatorRef, ty: CFIndex, value: *const c_void) -> CFNumberRef;
    pub fn CFNumberGetValue(n: CFNumberRef, ty: CFIndex, value: *mut c_void) -> Boolean;

    pub fn CFDictionaryCreate(
        alloc: CFAllocatorRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        n: CFIndex,
        key_cb: *const CFDictionaryKeyCallBacks,
        value_cb: *const CFDictionaryValueCallBacks,
    ) -> CFDictionaryRef;
    pub fn CFDictionaryGetValue(d: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;

    pub fn CFArrayGetCount(a: CFArrayRef) -> CFIndex;
    pub fn CFArrayGetValueAtIndex(a: CFArrayRef, i: CFIndex) -> CFTypeRef;
    pub fn CFSetGetCount(s: CFSetRef) -> CFIndex;
    pub fn CFSetGetValues(s: CFSetRef, values: *mut CFTypeRef);

    pub fn CFAttributedStringCreate(alloc: CFAllocatorRef, s: CFStringRef, attrs: CFDictionaryRef) -> CFAttributedStringRef;
    pub fn CFAttributedStringCreateMutableCopy(
        alloc: CFAllocatorRef,
        max: CFIndex,
        s: CFAttributedStringRef,
    ) -> CFMutableAttributedStringRef;
    pub fn CFAttributedStringSetAttribute(s: CFMutableAttributedStringRef, range: CFRange, name: CFStringRef, value: CFTypeRef);

    pub fn CFURLCreateFromFileSystemRepresentation(
        alloc: CFAllocatorRef,
        buf: *const u8,
        len: CFIndex,
        is_dir: Boolean,
    ) -> CFURLRef;

    pub fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    pub fn CFRunLoopRun();
    pub fn CFRunLoopStop(rl: CFRunLoopRef);
    pub fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    pub fn CFMachPortCreateRunLoopSource(alloc: CFAllocatorRef, port: CFMachPortRef, order: CFIndex) -> CFRunLoopSourceRef;
}

// ------------------------------------------------------------ CoreGraphics --

pub type CGFloat = f64;
pub type CGContextRef = *mut c_void;
pub type CGColorSpaceRef = *mut c_void;
pub type CGColorRef = *mut c_void;
pub type CGImageRef = *mut c_void;
pub type CGPathRef = *const c_void;
pub type CGGradientRef = *mut c_void;
pub type CGEventRef = *mut c_void;
pub type CGEventSourceRef = *mut c_void;
pub type CGImageDestinationRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGPoint {
    pub x: CGFloat,
    pub y: CGFloat,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGSize {
    pub width: CGFloat,
    pub height: CGFloat,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

impl CGRect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect { origin: CGPoint { x, y }, size: CGSize { width: w, height: h } }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CGAffineTransform {
    pub a: CGFloat,
    pub b: CGFloat,
    pub c: CGFloat,
    pub d: CGFloat,
    pub tx: CGFloat,
    pub ty: CGFloat,
}

pub const kCGImageAlphaPremultipliedFirst: u32 = 2;
pub const kCGBitmapByteOrder32Little: u32 = 2 << 12;
pub const kCGGradientDrawsBeforeStartLocation: u32 = 1;
pub const kCGGradientDrawsAfterEndLocation: u32 = 2;
pub const kCGBlendModeNormal: i32 = 0;
pub const kCGBlendModeSourceIn: i32 = 18;
pub const kCGBlendModeDestinationIn: i32 = 22;
pub const kCGLineCapRound: i32 = 1;

// Events.
pub const kCGSessionEventTap: u32 = 1;
pub const kCGHIDEventTap: u32 = 0;
pub const kCGHeadInsertEventTap: u32 = 0;
pub const kCGEventTapOptionDefault: u32 = 0;
pub const kCGEventKeyDown: u32 = 10;
pub const kCGEventKeyUp: u32 = 11;
pub const kCGEventFlagsChanged: u32 = 12;
pub const kCGEventTapDisabledByTimeout: u32 = 0xFFFF_FFFE;
pub const kCGEventTapDisabledByUserInput: u32 = 0xFFFF_FFFF;
pub const kCGKeyboardEventAutorepeat: u32 = 8;
pub const kCGKeyboardEventKeycode: u32 = 9;
pub const kCGEventSourceUserData: u32 = 42;
pub const kCGEventSourceStateHIDSystemState: i32 = 1;
pub const kCGEventFlagMaskShift: u64 = 0x0002_0000;
pub const kCGEventFlagMaskControl: u64 = 0x0004_0000;
pub const kCGEventFlagMaskAlternate: u64 = 0x0008_0000;
pub const kCGEventFlagMaskCommand: u64 = 0x0010_0000;
pub const kCGEventFlagMaskSecondaryFn: u64 = 0x0080_0000;
/// Device-dependent modifier bits (IOKit's NX_DEVICE*KEYMASK).
pub const NX_DEVICERCMDKEYMASK: u64 = 0x10;
pub const NX_DEVICERALTKEYMASK: u64 = 0x40;

pub type CGEventTapCallBack = unsafe extern "C" fn(proxy: *mut c_void, ty: u32, event: CGEventRef, user: *mut c_void) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub static kCGColorSpaceSRGB: CFStringRef;

    pub fn CGColorSpaceCreateWithName(name: CFStringRef) -> CGColorSpaceRef;
    pub fn CGColorSpaceRelease(cs: CGColorSpaceRef);
    pub fn CGColorCreateSRGB(r: CGFloat, g: CGFloat, b: CGFloat, a: CGFloat) -> CGColorRef;
    pub fn CGColorRelease(c: CGColorRef);

    pub fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CGColorSpaceRef,
        info: u32,
    ) -> CGContextRef;
    pub fn CGBitmapContextGetData(c: CGContextRef) -> *mut c_void;
    pub fn CGBitmapContextGetBytesPerRow(c: CGContextRef) -> usize;
    pub fn CGBitmapContextCreateImage(c: CGContextRef) -> CGImageRef;
    pub fn CGContextRelease(c: CGContextRef);
    pub fn CGImageRelease(i: CGImageRef);
    pub fn CGContextDrawImage(c: CGContextRef, r: CGRect, image: CGImageRef);
    pub fn CGImageCreateWithImageInRect(image: CGImageRef, r: CGRect) -> CGImageRef;
    pub fn CGDataProviderCreateWithData(
        info: *mut c_void,
        data: *const c_void,
        size: usize,
        release: Option<unsafe extern "C" fn(*mut c_void, *const c_void, usize)>,
    ) -> *mut c_void;
    pub fn CGDataProviderRelease(p: *mut c_void);
    /// `intent`: kCGRenderingIntentDefault = 0.
    pub fn CGImageCreateWithPNGDataProvider(source: *mut c_void, decode: *const f64, interpolate: bool, intent: i32) -> CGImageRef;

    pub fn CGContextSaveGState(c: CGContextRef);
    pub fn CGContextRestoreGState(c: CGContextRef);
    pub fn CGContextTranslateCTM(c: CGContextRef, tx: CGFloat, ty: CGFloat);
    pub fn CGContextScaleCTM(c: CGContextRef, sx: CGFloat, sy: CGFloat);
    pub fn CGContextClearRect(c: CGContextRef, r: CGRect);
    pub fn CGContextSetRGBFillColor(c: CGContextRef, r: CGFloat, g: CGFloat, b: CGFloat, a: CGFloat);
    pub fn CGContextSetRGBStrokeColor(c: CGContextRef, r: CGFloat, g: CGFloat, b: CGFloat, a: CGFloat);
    pub fn CGContextSetLineWidth(c: CGContextRef, w: CGFloat);
    pub fn CGContextSetLineCap(c: CGContextRef, cap: i32);
    pub fn CGContextFillRect(c: CGContextRef, r: CGRect);
    pub fn CGContextFillEllipseInRect(c: CGContextRef, r: CGRect);
    pub fn CGContextStrokeEllipseInRect(c: CGContextRef, r: CGRect);
    pub fn CGContextBeginPath(c: CGContextRef);
    pub fn CGContextAddPath(c: CGContextRef, p: CGPathRef);
    pub fn CGContextAddEllipseInRect(c: CGContextRef, r: CGRect);
    pub fn CGContextFillPath(c: CGContextRef);
    pub fn CGContextStrokePath(c: CGContextRef);
    pub fn CGContextMoveToPoint(c: CGContextRef, x: CGFloat, y: CGFloat);
    pub fn CGContextAddLineToPoint(c: CGContextRef, x: CGFloat, y: CGFloat);
    pub fn CGContextReplacePathWithStrokedPath(c: CGContextRef);
    pub fn CGContextClip(c: CGContextRef);
    pub fn CGContextEOFillPath(c: CGContextRef);
    pub fn CGContextClipToRect(c: CGContextRef, r: CGRect);
    pub fn CGContextSetBlendMode(c: CGContextRef, mode: i32);
    pub fn CGContextBeginTransparencyLayer(c: CGContextRef, aux: CFDictionaryRef);
    pub fn CGContextBeginTransparencyLayerWithRect(c: CGContextRef, r: CGRect, aux: CFDictionaryRef);
    pub fn CGContextEndTransparencyLayer(c: CGContextRef);
    pub fn CGContextDrawLinearGradient(c: CGContextRef, g: CGGradientRef, start: CGPoint, end: CGPoint, options: u32);
    pub fn CGContextDrawRadialGradient(
        c: CGContextRef,
        g: CGGradientRef,
        start_center: CGPoint,
        start_radius: CGFloat,
        end_center: CGPoint,
        end_radius: CGFloat,
        options: u32,
    );
    pub fn CGContextSetTextMatrix(c: CGContextRef, t: CGAffineTransform);
    pub fn CGContextSetTextPosition(c: CGContextRef, x: CGFloat, y: CGFloat);
    pub fn CGContextSetShouldSmoothFonts(c: CGContextRef, b: bool);
    pub fn CGContextSetAllowsFontSmoothing(c: CGContextRef, b: bool);
    pub fn CGContextSetShouldAntialias(c: CGContextRef, b: bool);
    pub fn CGContextSetInterpolationQuality(c: CGContextRef, q: i32);

    pub fn CGGradientCreateWithColorComponents(
        space: CGColorSpaceRef,
        components: *const CGFloat,
        locations: *const CGFloat,
        count: usize,
    ) -> CGGradientRef;
    pub fn CGGradientRelease(g: CGGradientRef);
    pub fn CGPathCreateWithRoundedRect(r: CGRect, cw: CGFloat, ch: CGFloat, t: *const CGAffineTransform) -> CGPathRef;
    pub fn CGPathRelease(p: CGPathRef);

    pub fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        mask: u64,
        callback: CGEventTapCallBack,
        user: *mut c_void,
    ) -> CFMachPortRef;
    pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    pub fn CGEventGetIntegerValueField(e: CGEventRef, field: u32) -> i64;
    pub fn CGEventSetIntegerValueField(e: CGEventRef, field: u32, value: i64);
    pub fn CGEventGetFlags(e: CGEventRef) -> u64;
    pub fn CGEventSetFlags(e: CGEventRef, flags: u64);
    pub fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, key: u16, down: bool) -> CGEventRef;
    pub fn CGEventPost(tap: u32, e: CGEventRef);
    pub fn CGEventSourceFlagsState(state: i32) -> u64;
    pub fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    pub fn CGImageDestinationCreateWithURL(url: CFURLRef, ty: CFStringRef, count: usize, options: CFDictionaryRef) -> CGImageDestinationRef;
    pub fn CGImageDestinationAddImage(d: CGImageDestinationRef, image: CGImageRef, props: CFDictionaryRef);
    pub fn CGImageDestinationFinalize(d: CGImageDestinationRef) -> bool;
}

// ---------------------------------------------------------------- CoreText --

pub type CTFontRef = *const c_void;
pub type CTLineRef = *const c_void;
pub type CTTypesetterRef = *const c_void;
pub type CTFontDescriptorRef = *const c_void;

pub const kCTLineTruncationEnd: u32 = 1;
pub const kCTFontBoldTrait: u32 = 1 << 1;

#[link(name = "CoreText", kind = "framework")]
unsafe extern "C" {
    pub static kCTFontAttributeName: CFStringRef;
    pub static kCTForegroundColorAttributeName: CFStringRef;

    pub fn CTFontCreateWithFontDescriptor(d: CTFontDescriptorRef, size: CGFloat, matrix: *const CGAffineTransform) -> CTFontRef;
    pub fn CTFontCreateCopyWithSymbolicTraits(
        font: CTFontRef,
        size: CGFloat,
        matrix: *const CGAffineTransform,
        value: u32,
        mask: u32,
    ) -> CTFontRef;
    pub fn CTFontCopyFamilyName(font: CTFontRef) -> CFStringRef;
    pub fn CTFontGetAscent(font: CTFontRef) -> CGFloat;
    pub fn CTFontGetDescent(font: CTFontRef) -> CGFloat;
    pub fn CTFontGetLeading(font: CTFontRef) -> CGFloat;
    pub fn CTFontManagerCreateFontDescriptorsFromURL(url: CFURLRef) -> CFArrayRef;

    pub fn CTLineCreateWithAttributedString(s: CFAttributedStringRef) -> CTLineRef;
    pub fn CTLineCreateTruncatedLine(line: CTLineRef, width: f64, ty: u32, token: CTLineRef) -> CTLineRef;
    pub fn CTLineGetTypographicBounds(line: CTLineRef, ascent: *mut CGFloat, descent: *mut CGFloat, leading: *mut CGFloat) -> f64;
    pub fn CTLineGetOffsetForStringIndex(line: CTLineRef, index: CFIndex, secondary: *mut CGFloat) -> CGFloat;
    pub fn CTLineDraw(line: CTLineRef, c: CGContextRef);

    pub fn CTTypesetterCreateWithAttributedString(s: CFAttributedStringRef) -> CTTypesetterRef;
    pub fn CTTypesetterSuggestLineBreak(t: CTTypesetterRef, start: CFIndex, width: f64) -> CFIndex;
    pub fn CTTypesetterCreateLine(t: CTTypesetterRef, range: CFRange) -> CTLineRef;
}

// ----------------------------------------------------------- Accessibility --

pub type AXUIElementRef = *const c_void;
pub type AXError = i32;
pub const kAXErrorSuccess: AXError = 0;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    pub static kAXTrustedCheckOptionPrompt: CFStringRef;
    pub fn AXIsProcessTrusted() -> Boolean;
    pub fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> Boolean;
    pub fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    pub fn AXUIElementCopyAttributeValue(e: AXUIElementRef, attr: CFStringRef, value: *mut CFTypeRef) -> AXError;
    pub fn AXUIElementIsAttributeSettable(e: AXUIElementRef, attr: CFStringRef, settable: *mut Boolean) -> AXError;
    pub fn AXUIElementSetMessagingTimeout(e: AXUIElementRef, seconds: f32) -> AXError;
    pub fn AXUIElementGetPid(e: AXUIElementRef, pid: *mut i32) -> AXError;
}

// --------------------------------------------------------------- IOKit HID --

pub type IOHIDManagerRef = *mut c_void;
pub type IOHIDDeviceRef = *mut c_void;
pub type IOReturn = i32;
pub const kIOReturnSuccess: IOReturn = 0;
pub const kIOHIDReportTypeOutput: u32 = 1;

pub type IOHIDReportCallback = unsafe extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    ty: u32,
    report_id: u32,
    report: *mut u8,
    len: CFIndex,
);
pub type IOHIDCallback = unsafe extern "C" fn(context: *mut c_void, result: IOReturn, sender: *mut c_void);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    pub fn IOHIDManagerCreate(alloc: CFAllocatorRef, options: u32) -> IOHIDManagerRef;
    pub fn IOHIDManagerSetDeviceMatching(m: IOHIDManagerRef, matching: CFDictionaryRef);
    pub fn IOHIDManagerCopyDevices(m: IOHIDManagerRef) -> CFSetRef;
    pub fn IOHIDDeviceGetProperty(d: IOHIDDeviceRef, key: CFStringRef) -> CFTypeRef;
    pub fn IOHIDDeviceOpen(d: IOHIDDeviceRef, options: u32) -> IOReturn;
    pub fn IOHIDDeviceClose(d: IOHIDDeviceRef, options: u32) -> IOReturn;
    pub fn IOHIDDeviceScheduleWithRunLoop(d: IOHIDDeviceRef, rl: CFRunLoopRef, mode: CFStringRef);
    pub fn IOHIDDeviceUnscheduleFromRunLoop(d: IOHIDDeviceRef, rl: CFRunLoopRef, mode: CFStringRef);
    pub fn IOHIDDeviceRegisterInputReportCallback(
        d: IOHIDDeviceRef,
        report: *mut u8,
        len: CFIndex,
        cb: Option<IOHIDReportCallback>,
        context: *mut c_void,
    );
    pub fn IOHIDDeviceRegisterRemovalCallback(d: IOHIDDeviceRef, cb: Option<IOHIDCallback>, context: *mut c_void);
    pub fn IOHIDDeviceSetReport(d: IOHIDDeviceRef, ty: u32, report_id: CFIndex, report: *const u8, len: CFIndex) -> IOReturn;
}

// ------------------------------------------------------------- libdispatch --

pub type DispatchFn = unsafe extern "C" fn(*mut c_void);

unsafe extern "C" {
    static _dispatch_main_q: u8;
    pub fn dispatch_async_f(queue: *const c_void, context: *mut c_void, work: DispatchFn);
    pub fn dispatch_sync_f(queue: *const c_void, context: *mut c_void, work: DispatchFn);
    pub fn dispatch_after_f(when: u64, queue: *const c_void, context: *mut c_void, work: DispatchFn);
    pub fn dispatch_time(when: u64, delta: i64) -> u64;
    pub fn pthread_main_np() -> i32;
}

pub fn main_queue() -> *const c_void {
    &raw const _dispatch_main_q as *const c_void
}

// ----------------------------------------------------------------- helpers --

/// An owned CoreFoundation object, released on drop.
pub struct Cf(pub CFTypeRef);

impl Cf {
    /// Takes ownership of a +1 reference; None for NULL.
    pub fn new(p: CFTypeRef) -> Option<Cf> {
        (!p.is_null()).then_some(Cf(p))
    }
    pub fn str(s: &str) -> Cf {
        Cf(cfstr(s))
    }
    pub fn as_ptr(&self) -> CFTypeRef {
        self.0
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

// SAFETY: CF objects used here are immutable or confined to one thread at a time.
unsafe impl Send for Cf {}

/// A new CFString (+1).
pub fn cfstr(s: &str) -> CFStringRef {
    unsafe { CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as CFIndex, kCFStringEncodingUTF8, 0) }
}

/// Copies a CFString into Rust; None if `s` is not a string.
pub fn string(s: CFTypeRef) -> Option<String> {
    if s.is_null() || unsafe { CFGetTypeID(s) != CFStringGetTypeID() } {
        return None;
    }
    unsafe {
        let len = CFStringGetLength(s);
        let max = CFStringGetMaximumSizeForEncoding(len, kCFStringEncodingUTF8) + 1;
        let mut buf = vec![0u8; max as usize];
        if CFStringGetCString(s, buf.as_mut_ptr() as *mut c_char, max, kCFStringEncodingUTF8) == 0 {
            return None;
        }
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(n);
        String::from_utf8(buf).ok()
    }
}

/// A CFNumber's integer value.
pub fn number(n: CFTypeRef) -> Option<i64> {
    if n.is_null() || unsafe { CFGetTypeID(n) != CFNumberGetTypeID() } {
        return None;
    }
    let mut v: i64 = 0;
    // kCFNumberSInt64Type = 4
    (unsafe { CFNumberGetValue(n, 4, &mut v as *mut i64 as *mut c_void) } != 0).then_some(v)
}

pub fn cfnum_i32(v: i32) -> Cf {
    Cf(unsafe { CFNumberCreate(std::ptr::null(), kCFNumberSInt32Type, &v as *const i32 as *const c_void) })
}

/// A CFDictionary of CF keys and values (+1).
pub fn dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> Cf {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
    Cf(unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as CFIndex,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    })
}

/// A file URL (+1).
pub fn file_url(path: &std::path::Path) -> Cf {
    let b = path.as_os_str().as_encoded_bytes();
    Cf(unsafe { CFURLCreateFromFileSystemRepresentation(std::ptr::null(), b.as_ptr(), b.len() as CFIndex, 0) })
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

pub fn is_main_thread() -> bool {
    unsafe { pthread_main_np() != 0 }
}
