#import <CoreGraphics/CoreGraphics.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>
#import <IOSurface/IOSurface.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <dispatch/dispatch.h>

#include <math.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>

typedef void (*RustyTeamsMacWindowFrameFn)(void* ctx,
                                           const uint8_t* bgra,
                                           size_t available_bytes,
                                           uint32_t width,
                                           uint32_t height,
                                           uint32_t stride);

static const size_t kMinCaptureDimension = 2;
static const size_t kMaxCaptureDimension = 16384;

static size_t clamp_dimension(double pixels) {
  if (!isfinite(pixels) || pixels < (double)kMinCaptureDimension) {
    return kMinCaptureDimension;
  }
  if (pixels > (double)kMaxCaptureDimension) {
    return kMaxCaptureDimension;
  }
  return (size_t)llround(pixels);
}

static double clamp_double(double value, double lo, double hi) {
  if (value < lo) {
    return lo;
  }
  if (value > hi) {
    return hi;
  }
  return value;
}

API_AVAILABLE(macos(14.0))
@interface RustyTeamsMacWindowSession
    : NSObject <SCStreamDelegate, SCStreamOutput, SCContentSharingPickerObserver>
- (instancetype)initWithMaxFps:(uint32_t)maxFps includeCursor:(BOOL)includeCursor;
- (void)startPicker;
- (void)setMaxFps:(uint32_t)maxFps;
- (int32_t)pollWithContext:(void*)ctx callback:(RustyTeamsMacWindowFrameFn)callback;
- (void)stop;
@end

API_AVAILABLE(macos(14.0))
@implementation RustyTeamsMacWindowSession {
  BOOL _stopped;
  BOOL _permanentError;
  BOOL _pickerFailed;
  BOOL _includeCursor;
  uint32_t _maxFps;
  SCStream* _stream;
  SCContentFilter* _filter;
  size_t _configuredWidth;
  size_t _configuredHeight;
  IOSurfaceRef _latestSurface;
  CGRect _latestCropRect;
  dispatch_queue_t _captureQueue;
}

- (instancetype)initWithMaxFps:(uint32_t)maxFps includeCursor:(BOOL)includeCursor {
  self = [super init];
  if (self) {
    _maxFps = maxFps;
    _includeCursor = includeCursor;
    _captureQueue =
        dispatch_queue_create("org.rusty-teams.sck-window", DISPATCH_QUEUE_SERIAL);
  }
  return self;
}

- (void)dealloc {
  if (_latestSurface != NULL) {
    IOSurfaceDecrementUseCount(_latestSurface);
    CFRelease(_latestSurface);
    _latestSurface = NULL;
  }
}

- (SCStreamConfiguration*)buildConfigWithWidth:(size_t)width
                                        height:(size_t)height
                                        maxFps:(uint32_t)maxFps
                                 includeCursor:(BOOL)includeCursor {
  SCStreamConfiguration* config = [[SCStreamConfiguration alloc] init];
  config.pixelFormat = kCVPixelFormatType_32BGRA;
  config.colorSpaceName = kCGColorSpaceSRGB;
  config.showsCursor = includeCursor;
  config.captureResolution = SCCaptureResolutionBest;
  config.width = width < kMinCaptureDimension ? kMinCaptureDimension : width;
  config.height = height < kMinCaptureDimension ? kMinCaptureDimension : height;
  config.minimumFrameInterval =
      maxFps > 0 ? CMTimeMake(1, (int32_t)maxFps) : kCMTimeZero;
  return config;
}

- (void)startPicker {
  SCContentSharingPicker* picker = [SCContentSharingPicker sharedPicker];
  if (picker.maximumStreamCount == nil ||
      [picker.maximumStreamCount unsignedIntValue] == 0) {
    picker.maximumStreamCount = @1;
  }
  [picker addObserver:self];
  picker.active = YES;
  SCContentSharingPickerConfiguration* config =
      [[SCContentSharingPickerConfiguration alloc] init];
  config.allowedPickerModes = SCContentSharingPickerModeSingleWindow;
  config.allowsChangingSelectedContent = YES;
  picker.defaultConfiguration = config;
  [picker presentPickerUsingContentStyle:SCShareableContentStyleWindow];
}

- (void)setMaxFps:(uint32_t)maxFps {
  SCStream* stream = nil;
  SCStreamConfiguration* config = nil;
  @synchronized(self) {
    if (_stopped || _maxFps == maxFps) {
      return;
    }
    _maxFps = maxFps;
    if (_stream != nil && _configuredWidth >= kMinCaptureDimension &&
        _configuredHeight >= kMinCaptureDimension) {
      stream = _stream;
      config = [self buildConfigWithWidth:_configuredWidth
                                   height:_configuredHeight
                                   maxFps:_maxFps
                            includeCursor:_includeCursor];
    }
  }
  if (stream != nil && config != nil) {
    [stream updateConfiguration:config completionHandler:nil];
  }
}

- (int32_t)pollWithContext:(void*)ctx callback:(RustyTeamsMacWindowFrameFn)callback {
  IOSurfaceRef surface = NULL;
  CGRect crop = CGRectZero;
  @synchronized(self) {
    if (_stopped) {
      return -1;
    }
    if (_pickerFailed) {
      return -2;
    }
    if (_permanentError) {
      return -1;
    }
    if (_latestSurface == NULL) {
      return 0;
    }
    surface = _latestSurface;
    crop = _latestCropRect;
    CFRetain(surface);
    IOSurfaceIncrementUseCount(surface);
  }

  int32_t result = 0;
  if (IOSurfaceLock(surface, kIOSurfaceLockReadOnly, NULL) == kIOReturnSuccess) {
    const uint8_t* base = (const uint8_t*)IOSurfaceGetBaseAddress(surface);
    size_t surfW = IOSurfaceGetWidth(surface);
    size_t surfH = IOSurfaceGetHeight(surface);
    size_t stride = IOSurfaceGetBytesPerRow(surface);
    size_t bpp = IOSurfaceGetBytesPerElement(surface);
    if (base != NULL && bpp == 4 && surfW >= kMinCaptureDimension &&
        surfH >= kMinCaptureDimension && stride >= surfW * 4) {
      size_t x = 0;
      size_t y = 0;
      size_t w = surfW;
      size_t h = surfH;
      if (!CGRectIsEmpty(crop)) {
        size_t cx = (size_t)crop.origin.x;
        size_t cy = (size_t)crop.origin.y;
        size_t cw = (size_t)crop.size.width;
        size_t ch = (size_t)crop.size.height;
        if (cx < surfW && cy < surfH && cw >= kMinCaptureDimension &&
            ch >= kMinCaptureDimension && cx + cw <= surfW && cy + ch <= surfH) {
          x = cx;
          y = cy;
          w = cw;
          h = ch;
        }
      }
      size_t offset = y * stride + x * 4;
      size_t totalBytes = surfH * stride;
      if (offset < totalBytes) {
        callback(ctx, base + offset, totalBytes - offset, (uint32_t)w, (uint32_t)h,
                 (uint32_t)stride);
        result = 1;
      }
    }
    IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
  }

  IOSurfaceDecrementUseCount(surface);
  CFRelease(surface);
  return result;
}

- (void)stop {
  SCStream* streamToStop = nil;
  IOSurfaceRef surfaceToRelease = NULL;
  @synchronized(self) {
    if (_stopped) {
      return;
    }
    _stopped = YES;
    streamToStop = _stream;
    _stream = nil;
    _filter = nil;
    surfaceToRelease = _latestSurface;
    _latestSurface = NULL;
  }

  SCContentSharingPicker* picker = [SCContentSharingPicker sharedPicker];
  [picker removeObserver:self];
  if (streamToStop != nil) {
    [streamToStop removeStreamOutput:self type:SCStreamOutputTypeScreen error:nil];
    [streamToStop stopCaptureWithCompletionHandler:nil];
  }
  picker.active = NO;

  if (surfaceToRelease != NULL) {
    IOSurfaceDecrementUseCount(surfaceToRelease);
    CFRelease(surfaceToRelease);
  }
}

- (void)maybeReconfigureWidth:(size_t)desiredW height:(size_t)desiredH {
  SCStream* stream = nil;
  SCStreamConfiguration* config = nil;
  @synchronized(self) {
    if (_stopped || _stream == nil) {
      return;
    }
    long dw = (long)desiredW - (long)_configuredWidth;
    long dh = (long)desiredH - (long)_configuredHeight;
    if (labs(dw) < 2 && labs(dh) < 2) {
      return;
    }
    _configuredWidth = desiredW;
    _configuredHeight = desiredH;
    stream = _stream;
    config = [self buildConfigWithWidth:desiredW
                                 height:desiredH
                                 maxFps:_maxFps
                          includeCursor:_includeCursor];
  }
  if (stream != nil && config != nil) {
    [stream updateConfiguration:config completionHandler:nil];
  }
}

- (void)contentSharingPicker:(SCContentSharingPicker*)picker
         didUpdateWithFilter:(SCContentFilter*)filter
                   forStream:(nullable SCStream*)stream {
  (void)picker;
  SCStream* existingStream = nil;
  SCStream* newStream = nil;
  SCStreamConfiguration* config = nil;
  dispatch_queue_t queue = nil;

  @synchronized(self) {
    if (_stopped) {
      return;
    }
    if (stream != nil && stream != _stream) {
      return;
    }

    CGRect contentRect = filter.contentRect;
    double scale = filter.pointPixelScale > 0.0f ? filter.pointPixelScale : 1.0;
    size_t width = clamp_dimension(contentRect.size.width * scale);
    size_t height = clamp_dimension(contentRect.size.height * scale);

    _filter = filter;
    _configuredWidth = width;
    _configuredHeight = height;
    config = [self buildConfigWithWidth:width
                                 height:height
                                 maxFps:_maxFps
                          includeCursor:_includeCursor];

    if (_stream != nil) {
      existingStream = _stream;
    } else {
      newStream = [[SCStream alloc] initWithFilter:filter
                                     configuration:config
                                          delegate:self];
      _stream = newStream;
      queue = _captureQueue;
    }
  }

  if (existingStream != nil) {
    [existingStream updateContentFilter:filter completionHandler:nil];
    [existingStream updateConfiguration:config completionHandler:nil];
    return;
  }

  if (newStream == nil) {
    @synchronized(self) {
      _pickerFailed = YES;
    }
    return;
  }

  // Rely on `picker.defaultConfiguration` rather than calling
  // `[picker setConfiguration:forStream:]`: mutating the per-stream picker
  // configuration dispatches an async `updateStream:withContentFilter:` IPC
  // whose filter no longer matches the picker-authorized filter in `replayd`,
  // causing `verifyContentPickerFilter:` to fail and trigger spurious Screen
  // Recording TCC prompts on macOS 27.

  NSError* error = nil;
  BOOL added = [newStream addStreamOutput:self
                                     type:SCStreamOutputTypeScreen
                       sampleHandlerQueue:queue
                                    error:&error];
  if (!added || error != nil) {
    @synchronized(self) {
      _pickerFailed = YES;
    }
    return;
  }

  __weak RustyTeamsMacWindowSession* weakSelf = self;
  [newStream startCaptureWithCompletionHandler:^(NSError* _Nullable startError) {
    if (startError == nil) {
      return;
    }
    RustyTeamsMacWindowSession* strongSelf = weakSelf;
    if (strongSelf == nil) {
      return;
    }
    @synchronized(strongSelf) {
      strongSelf->_permanentError = YES;
    }
  }];
}

- (void)contentSharingPicker:(SCContentSharingPicker*)picker
          didCancelForStream:(nullable SCStream*)stream {
  (void)picker;
  @synchronized(self) {
    if (_stopped) {
      return;
    }
    // Only treat cancelling the initial picker as cancellation; cancelling a
    // mid-share "Change Window" sheet keeps the active capture running.
    if (stream == nil && _stream == nil) {
      _permanentError = YES;
    }
  }
}

- (void)contentSharingPickerStartDidFailWithError:(NSError*)error {
  (void)error;
  @synchronized(self) {
    if (!_stopped && _stream == nil) {
      _pickerFailed = YES;
    }
  }
}

- (void)stream:(SCStream*)stream
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
                   ofType:(SCStreamOutputType)type {
  (void)stream;
  if (type != SCStreamOutputTypeScreen) {
    return;
  }
  CVImageBufferRef imageBuffer = CMSampleBufferGetImageBuffer(sampleBuffer);
  if (imageBuffer == NULL) {
    return;
  }
  IOSurfaceRef ioSurface = CVPixelBufferGetIOSurface(imageBuffer);
  if (ioSurface == NULL) {
    return;
  }
  CFArrayRef attachments =
      CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
  if (attachments == NULL || CFArrayGetCount(attachments) == 0) {
    return;
  }
  NSDictionary* dict =
      (__bridge NSDictionary*)CFArrayGetValueAtIndex(attachments, 0);
  if (dict == nil) {
    return;
  }

  NSNumber* statusNum = dict[SCStreamFrameInfoStatus];
  if (statusNum == nil) {
    return;
  }
  NSInteger status = [statusNum integerValue];
  if (status != SCFrameStatusComplete && status != SCFrameStatusStarted) {
    return;
  }

  double scaleFactor = 1.0;
  NSNumber* scaleNum = dict[SCStreamFrameInfoScaleFactor];
  if (scaleNum != nil && [scaleNum doubleValue] > 0.0) {
    scaleFactor = [scaleNum doubleValue];
  }

  double contentScale = 1.0;
  NSNumber* contentScaleNum = dict[SCStreamFrameInfoContentScale];
  if (contentScaleNum != nil && [contentScaleNum doubleValue] > 0.0) {
    contentScale = [contentScaleNum doubleValue];
  }

  CGRect contentRect = CGRectZero;
  id rectObj = dict[SCStreamFrameInfoContentRect];
  if (rectObj != nil) {
    if (!CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)rectObj,
                                                &contentRect)) {
      contentRect = CGRectZero;
    }
  }

  size_t surfW = IOSurfaceGetWidth(ioSurface);
  size_t surfH = IOSurfaceGetHeight(ioSurface);
  CGRect cropRect = CGRectZero;
  if (!CGRectIsEmpty(contentRect) && surfW >= kMinCaptureDimension &&
      surfH >= kMinCaptureDimension) {
    double rx = floor(contentRect.origin.x * scaleFactor);
    double ry = floor(contentRect.origin.y * scaleFactor);
    double rw = (double)llround(contentRect.size.width * scaleFactor);
    double rh = (double)llround(contentRect.size.height * scaleFactor);
    rx = clamp_double(rx, 0.0, (double)surfW);
    ry = clamp_double(ry, 0.0, (double)surfH);
    rw = clamp_double(rw, 0.0, (double)surfW - rx);
    rh = clamp_double(rh, 0.0, (double)surfH - ry);
    if (rw >= (double)kMinCaptureDimension && rh >= (double)kMinCaptureDimension) {
      cropRect = CGRectMake(rx, ry, rw, rh);
    }
  }

  if (!CGRectIsEmpty(contentRect)) {
    size_t desiredW =
        clamp_dimension((contentRect.size.width / contentScale) * scaleFactor);
    size_t desiredH =
        clamp_dimension((contentRect.size.height / contentScale) * scaleFactor);
    [self maybeReconfigureWidth:desiredW height:desiredH];
  }

  IOSurfaceRef oldSurface = NULL;
  @synchronized(self) {
    if (_stopped) {
      return;
    }
    CFRetain(ioSurface);
    IOSurfaceIncrementUseCount(ioSurface);
    oldSurface = _latestSurface;
    _latestSurface = ioSurface;
    _latestCropRect = cropRect;
  }

  if (oldSurface != NULL) {
    IOSurfaceDecrementUseCount(oldSurface);
    CFRelease(oldSurface);
  }
}

- (void)stream:(SCStream*)stream didStopWithError:(NSError*)error {
  (void)error;
  @synchronized(self) {
    if (!_stopped && stream == _stream) {
      _permanentError = YES;
    }
  }
}

@end

bool rusty_teams_mac_sck_window_available(void) {
  if (@available(macOS 14.0, *)) {
    return true;
  }
  return false;
}

void* rusty_teams_mac_window_capture_start(uint32_t max_fps, bool include_cursor) {
  @autoreleasepool {
    if (@available(macOS 14.0, *)) {
      RustyTeamsMacWindowSession* session =
          [[RustyTeamsMacWindowSession alloc] initWithMaxFps:max_fps
                                               includeCursor:include_cursor ? YES : NO];
      [session startPicker];
      return (void*)CFBridgingRetain(session);
    }
    return NULL;
  }
}

void rusty_teams_mac_window_capture_set_fps(void* handle, uint32_t max_fps) {
  if (handle == NULL) {
    return;
  }
  @autoreleasepool {
    if (@available(macOS 14.0, *)) {
      RustyTeamsMacWindowSession* session =
          (__bridge RustyTeamsMacWindowSession*)handle;
      [session setMaxFps:max_fps];
    }
  }
}

int32_t rusty_teams_mac_window_capture_poll(void* handle,
                                            void* ctx,
                                            RustyTeamsMacWindowFrameFn callback) {
  if (handle == NULL || callback == NULL) {
    return -2;
  }
  @autoreleasepool {
    if (@available(macOS 14.0, *)) {
      RustyTeamsMacWindowSession* session =
          (__bridge RustyTeamsMacWindowSession*)handle;
      return [session pollWithContext:ctx callback:callback];
    }
    return -2;
  }
}

void rusty_teams_mac_window_capture_stop(void* handle) {
  if (handle == NULL) {
    return;
  }
  @autoreleasepool {
    if (@available(macOS 14.0, *)) {
      RustyTeamsMacWindowSession* session =
          (RustyTeamsMacWindowSession*)CFBridgingRelease(handle);
      [session stop];
    }
  }
}
