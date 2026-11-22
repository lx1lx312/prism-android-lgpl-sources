/*
 * prism_usbhost_io.c — i trasferimenti dell'iolib prism_usbhost (M8.1): bulk, interrupt, control, clear halt, reset,
 * update, sull'handle libusb avvolto attorno all'fd di UsbManager (prism_usbhost.c). Le stesse richieste di
 * libgphoto2_port/libusb1/libusb1.c 2.5.34 (bmRequestType, codici d'errore), con una differenza: gli eventi PTP
 * (endpoint interrupt) si leggono con un libusb_interrupt_transfer sincrono a tempo invece della coda di URB
 * asincroni di usb1 — un evento non letto resta nella camera (l'endpoint IN risponde NAK finche' l'host non chiede),
 * quindi fra due check_int non si perde niente.
 */
#include "prism_usbhost_private.h"

#include <string.h>

int
prism_usbhost_map_error (int r, int fallback)
{
	switch (r) {
	case LIBUSB_SUCCESS:             return GP_OK;
	case LIBUSB_ERROR_INVALID_PARAM: return GP_ERROR_BAD_PARAMETERS;
	case LIBUSB_ERROR_NO_DEVICE:     return GP_ERROR_IO_USB_FIND;
	case LIBUSB_ERROR_TIMEOUT:       return GP_ERROR_TIMEOUT;
	case LIBUSB_ERROR_NO_MEM:        return GP_ERROR_NO_MEMORY;
	case LIBUSB_ERROR_NOT_SUPPORTED: return GP_ERROR_NOT_SUPPORTED;
	default:                         return fallback;
	}
}

static int
not_open (GPPort *port)
{
	if (port && port->pl && port->pl->dh) return 0;
	if (port) gp_port_set_error (port, "prism_usbhost: the port is not open");
	return 1;
}

static int
usbhost_write (GPPort *port, const char *bytes, int size)
{
	if (not_open (port)) return GP_ERROR_BAD_PARAMETERS;
	int done = 0;
	int r = libusb_bulk_transfer (port->pl->dh, (unsigned char)port->settings.usb.outep, (unsigned char *)bytes, size,
	                              &done, port->timeout);
	if (r != LIBUSB_SUCCESS) {
		gp_log (GP_LOG_DEBUG, PRISM_USBHOST_DOMAIN, "bulk out %d bytes: %s", size, libusb_error_name (r));
		return prism_usbhost_map_error (r, GP_ERROR_IO_WRITE);
	}
	return done;
}

static int
usbhost_read (GPPort *port, char *bytes, int size)
{
	if (not_open (port)) return GP_ERROR_BAD_PARAMETERS;
	int done = 0;
	int r = libusb_bulk_transfer (port->pl->dh, (unsigned char)port->settings.usb.inep, (unsigned char *)bytes, size,
	                              &done, port->timeout);
	if (r != LIBUSB_SUCCESS) {
		gp_log (GP_LOG_DEBUG, PRISM_USBHOST_DOMAIN, "bulk in %d bytes: %s", size, libusb_error_name (r));
		return prism_usbhost_map_error (r, GP_ERROR_IO_READ);
	}
	return done;
}

static int
usbhost_check_int (GPPort *port, char *bytes, int size, int timeout)
{
	if (not_open (port) || timeout < 0) return GP_ERROR_BAD_PARAMETERS;
	if (!port->settings.usb.intep) return GP_ERROR_NOT_SUPPORTED;
	int done = 0;
	/* 0 per libusb vuol dire «senza limite»; per libgphoto2 «guarda e torna». */
	int r = libusb_interrupt_transfer (port->pl->dh, (unsigned char)port->settings.usb.intep, (unsigned char *)bytes,
	                                   size, &done, timeout ? (unsigned)timeout : 1u);
	if (r == LIBUSB_ERROR_TIMEOUT && done == 0) return GP_ERROR_TIMEOUT;
	if (r != LIBUSB_SUCCESS && r != LIBUSB_ERROR_TIMEOUT) return prism_usbhost_map_error (r, GP_ERROR_IO_READ);
	return done;
}

static int
control (GPPort *port, int type, int request, int value, int index, char *bytes, int size, int fallback)
{
	if (not_open (port)) return GP_ERROR_BAD_PARAMETERS;
	int r = libusb_control_transfer (port->pl->dh, (uint8_t)type, (uint8_t)request, (uint16_t)value, (uint16_t)index,
	                                 (unsigned char *)bytes, (uint16_t)size, port->timeout);
	return r < 0 ? prism_usbhost_map_error (r, fallback) : r;
}

static int
msg_write (GPPort *p, int rq, int v, int i, char *b, int n)
{
	return control (p, LIBUSB_REQUEST_TYPE_VENDOR | LIBUSB_RECIPIENT_DEVICE, rq, v, i, b, n, GP_ERROR_IO_WRITE);
}
static int
msg_read (GPPort *p, int rq, int v, int i, char *b, int n)
{
	return control (p, LIBUSB_REQUEST_TYPE_VENDOR | LIBUSB_RECIPIENT_DEVICE | LIBUSB_ENDPOINT_IN, rq, v, i, b, n,
	                GP_ERROR_IO_READ);
}
static int
msg_interface_write (GPPort *p, int rq, int v, int i, char *b, int n)
{
	return control (p, LIBUSB_REQUEST_TYPE_VENDOR | LIBUSB_RECIPIENT_INTERFACE, rq, v, i, b, n, GP_ERROR_IO_WRITE);
}
static int
msg_interface_read (GPPort *p, int rq, int v, int i, char *b, int n)
{
	return control (p, LIBUSB_REQUEST_TYPE_VENDOR | LIBUSB_RECIPIENT_INTERFACE | LIBUSB_ENDPOINT_IN, rq, v, i, b, n,
	                GP_ERROR_IO_READ);
}
/* PTP: Cancel (0x64), Get Device Status (0x67), Device Reset (0x66) — camlibs/ptp2/usb.c. */
static int
msg_class_write (GPPort *p, int rq, int v, int i, char *b, int n)
{
	return control (p, LIBUSB_REQUEST_TYPE_CLASS | LIBUSB_RECIPIENT_INTERFACE, rq, v, i, b, n, GP_ERROR_IO_WRITE);
}
static int
msg_class_read (GPPort *p, int rq, int v, int i, char *b, int n)
{
	return control (p, LIBUSB_REQUEST_TYPE_CLASS | LIBUSB_RECIPIENT_INTERFACE | LIBUSB_ENDPOINT_IN, rq, v, i, b, n,
	                GP_ERROR_IO_READ);
}

static int
usbhost_clear_halt (GPPort *port, int ep)
{
	if (not_open (port)) return GP_ERROR_BAD_PARAMETERS;
	int address;
	switch (ep) {
	case GP_PORT_USB_ENDPOINT_IN:  address = port->settings.usb.inep; break;
	case GP_PORT_USB_ENDPOINT_OUT: address = port->settings.usb.outep; break;
	case GP_PORT_USB_ENDPOINT_INT: address = port->settings.usb.intep; break;
	default:
		gp_port_set_error (port, "prism_usbhost: bad endpoint 0x%x", ep);
		return GP_ERROR_BAD_PARAMETERS;
	}
	int r = libusb_clear_halt (port->pl->dh, (unsigned char)address);
	return r == LIBUSB_SUCCESS ? GP_OK : prism_usbhost_map_error (r, GP_ERROR_IO_USB_CLEAR_HALT);
}

static int
usbhost_reset (GPPort *port)
{
	if (not_open (port)) return GP_ERROR_BAD_PARAMETERS;
	int r = libusb_reset_device (port->pl->dh);
	return r == LIBUSB_SUCCESS ? GP_OK : prism_usbhost_map_error (r, GP_ERROR_IO);
}

/* gp_port_set_settings: il nome della porta sempre; interfaccia / altsetting solo sull'handle aperto (come usb1). La
 * configurazione non si cambia: la sceglie Android all'apertura (UsbDeviceConnection), qui resta quella attiva. */
static int
usbhost_update (GPPort *port)
{
	if (!port || !port->pl) return GP_ERROR_BAD_PARAMETERS;
	GPPortSettingsUSB *now = &port->settings.usb, *next = &port->settings_pending.usb;
	memcpy (now->port, next->port, sizeof (now->port));
	if (!port->pl->dh) return GP_OK;
	if (next->interface != now->interface || next->altsetting != now->altsetting) {
		if (next->interface != now->interface) {
			if (port->pl->claimed) libusb_release_interface (port->pl->dh, now->interface);
			port->pl->claimed = 0;
			int r = libusb_claim_interface (port->pl->dh, next->interface);
			if (r != LIBUSB_SUCCESS) return prism_usbhost_map_error (r, GP_ERROR_IO_USB_CLAIM);
			port->pl->claimed = 1;
		}
		int r = libusb_set_interface_alt_setting (port->pl->dh, next->interface, next->altsetting);
		if (r != LIBUSB_SUCCESS) return prism_usbhost_map_error (r, GP_ERROR_IO_UPDATE);
	}
	int config = now->config;
	*now = *next;
	now->config = config;
	return GP_OK;
}

void
prism_usbhost_io_operations (GPPortOperations *ops)
{
	ops->read = usbhost_read;
	ops->write = usbhost_write;
	ops->check_int = usbhost_check_int;
	ops->update = usbhost_update;
	ops->clear_halt = usbhost_clear_halt;
	ops->reset = usbhost_reset;
	ops->msg_write = msg_write;
	ops->msg_read = msg_read;
	ops->msg_interface_write = msg_interface_write;
	ops->msg_interface_read = msg_interface_read;
	ops->msg_class_write = msg_class_write;
	ops->msg_class_read = msg_class_read;
}
