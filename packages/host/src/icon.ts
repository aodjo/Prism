/**
 * The tray icon, as a data URL.
 *
 * Embedded rather than loaded from disk because a packaged application resolves paths
 * differently from a development run, and an icon that fails to load leaves an invisible
 * tray item with no way to click it.
 *
 * Drawn as a black silhouette so macOS treats it as a template and tints it for whichever
 * menu bar appearance is active.
 */
export const TRAY_ICON =
  'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAAAa0lEQVR42u2WMQrAMAwD/f9PX5dOIaXFkSNKLNBo6+QsiWi1fi5unwnA4LMAxtDtEFaAp7BtEFaAt5ByCCvA1+UlECQAcLQvuQILADjaS6+AAABHe8k8QgDC9YbZPRQAkB1S2hpu/8C2WlNdq8+DfVVdpFcAAAAASUVORK5CYII=';
