/*
 * prism_usbhost.c — l'iolib «prism_usbhost» di libgphoto2_port per Android (M8.1, docs/ANDROID-GPHOTO2.md §4):
 * una porta USB vera sopra l'fd di usbfs che UsbManager da' all'app (libusb_wrap_sys_device, libusb 1.0.30).
 *
 * Perche' non l'iolib usb1 di libgphoto2: 2.5.34 ha gia' una via Android (gp_port_usb_set_sys_device(fd), un fd
 * GLOBALE avvolto all'elenco delle porte), ma (1) se libusb_wrap_sys_device fallisce, has_external_fd() chiama
 * libusb_get_device(NULL) e il processo cade — un fd sbagliato o staccato = crash; (2) un solo fd per processo,
 * avvolto una volta e mai rilasciato (una camera staccata e ricollegata ha un fd nuovo); (3) l'fd serve PRIMA
 * dell'elenco, cioe' permesso e apertura prima di sapere che camera e'. Qui invece:
 *   · gp_port_library_list   le camere del trasporto (UsbManager), «usb:USBHOST,<deviceId>», senza aprire niente;
 *   · find_device(_by_class) rispondono da vendor / product / classe dell'elenco: gp_abilities_list_detect li chiama
 *                            per ogni modello (~2900) e gp_camera_init PRIMA di open — nessun fd, nessun permesso;
 *   · open                   il permesso (se manca) e l'fd dal trasporto, libusb_wrap_sys_device in un contesto
 *                            NO_DEVICE_DISCOVERY tutto suo, i descrittori → settings.usb (endpoint, interfaccia,
 *                            maxpacketsize: ptp2 li legge dopo open), claim dell'interfaccia;
 *   · close                  libusb_close (l'fd avvolto NON si chiude: e' del guscio), libusb_exit, t->close.
 * I trasferimenti stanno in prism_usbhost_io.c.
 */
#include "prism_usbhost_private.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const prism_usbhost_transport *registered = NULL;

__attribute__((visibility("default"))) void
prism_usbhost_register_transport (const prism_usbhost_transport *transport)
{
	if (transport && (transport->version != PRISM_USBHOST_TRANSPORT_VERSION || !transport->list
	                  || !transport->open || !transport->close)) {
		gp_log (GP_LOG_ERROR, PRISM_USBHOST_DOMAIN, "transport refused (version %u, expected %u, or incomplete)",
		        transport->version, PRISM_USBHOST_TRANSPORT_VERSION);
		return;
	}
	registered = transport;
}

/* «usb:USBHOST,17» → 17; -1 se la porta non e' nostra. */
static long
device_of_path (const char *path)
{
	size_t n = strlen (PRISM_USBHOST_PREFIX);
	if (!path || strncmp (path, PRISM_USBHOST_PREFIX, n) != 0) return -1;
	char *end = NULL;
	long id = strtol (path + n, &end, 10);
	if (end == path + n || *end != '\0' || id < 0) return -1;
	return id;
}

/* La camera di questa porta nell'elenco del trasporto, adesso. 0 se c'e'. */
static int
lookup (GPPort *port, prism_usbhost_device_info *out)
{
	if (!port->pl || !port->pl->t) return -1;
	long id = device_of_path (port->settings.usb.port);
	if (id < 0) return -1;
	prism_usbhost_device_info devices[PRISM_USBHOST_MAX_DEVICES];
	int n = port->pl->t->list (devices, PRISM_USBHOST_MAX_DEVICES);
	for (int i = 0; i < n && i < PRISM_USBHOST_MAX_DEVICES; i++)
		if (devices[i].device_id == (uint32_t)id) { *out = devices[i]; return 0; }
	return -1;
}

GPPortType
gp_port_library_type (void)
{
	return GP_PORT_USB;
}

int
gp_port_library_list (GPPortInfoList *list)
{
	const prism_usbhost_transport *t = registered;
	if (!t) {
		gp_log (GP_LOG_DEBUG, PRISM_USBHOST_DOMAIN, "no USB Host transport registered in this process: no port");
		return GP_OK;
	}
	prism_usbhost_device_info devices[PRISM_USBHOST_MAX_DEVICES];
	int n = t->list (devices, PRISM_USBHOST_MAX_DEVICES);
	for (int i = 0; i < n && i < PRISM_USBHOST_MAX_DEVICES; i++) {
		GPPortInfo info;
		char path[64];
		snprintf (path, sizeof (path), PRISM_USBHOST_PREFIX "%u", devices[i].device_id);
		devices[i].name[sizeof (devices[i].name) - 1] = '\0';
		if (gp_port_info_new (&info) < GP_OK) return GP_ERROR_NO_MEMORY;
		gp_port_info_set_type (info, GP_PORT_USB);
		gp_port_info_set_name (info, devices[i].name[0] ? devices[i].name : "USB camera");
		gp_port_info_set_path (info, path);
		int r = gp_port_info_list_append (list, info);
		if (r < GP_OK) return r;
	}
	return GP_OK;
}

static int
usbhost_init (GPPort *port)
{
	port->pl = calloc (1, sizeof (GPPortPrivateLibrary));
	if (!port->pl) return GP_ERROR_NO_MEMORY;
	port->pl->t = registered;
	return GP_OK;
}

static int usbhost_close (GPPort *port);

static int
usbhost_exit (GPPort *port)
{
	if (!port->pl) return GP_OK;
	usbhost_close (port);
	free (port->pl);
	port->pl = NULL;
	return GP_OK;
}

static int
usbhost_find_device (GPPort *port, int idvendor, int idproduct)
{
	prism_usbhost_device_info d;
	if (!idvendor || lookup (port, &d) != 0) return GP_ERROR_IO_USB_FIND;
	if (d.vendor_id != (uint16_t)idvendor || d.product_id != (uint16_t)idproduct) return GP_ERROR_IO_USB_FIND;
	port->pl->want_class = port->pl->want_subclass = port->pl->want_protocol = 0;
	return GP_OK;
}

static int
usbhost_find_device_by_class (GPPort *port, int class, int subclass, int protocol)
{
	prism_usbhost_device_info d;
	if (!class || lookup (port, &d) != 0 || !d.iface_class) return GP_ERROR_IO_USB_FIND;
	if (d.iface_class != class || (subclass != -1 && d.iface_subclass != subclass)
	    || (protocol != -1 && d.iface_protocol != protocol))
		return GP_ERROR_IO_USB_FIND;
	port->pl->want_class = class;
	port->pl->want_subclass = subclass;
	port->pl->want_protocol = protocol;
	return GP_OK;
}

/* L'interfaccia di cattura nei descrittori (la classe chiesta, o 6 Still Image, o la prima con due bulk) e i suoi
 * endpoint → settings.usb. GP_OK o GP_ERROR_IO_USB_FIND. */
static int
choose_interface (GPPort *port, const struct libusb_config_descriptor *cfg)
{
	const GPPortPrivateLibrary *pl = port->pl;
	int want = pl->want_class ? pl->want_class : LIBUSB_CLASS_IMAGE;
	for (int pass = 0; pass < 2; pass++)
		for (int i = 0; i < cfg->bNumInterfaces; i++)
			for (int a = 0; a < cfg->interface[i].num_altsetting; a++) {
				const struct libusb_interface_descriptor *d = &cfg->interface[i].altsetting[a];
				if (pass == 0 && d->bInterfaceClass != want) continue;
				int in = 0, out = 0, irq = 0, mps = 0;
				for (int e = 0; e < d->bNumEndpoints; e++) {
					const struct libusb_endpoint_descriptor *ep = &d->endpoint[e];
					int type = ep->bmAttributes & LIBUSB_TRANSFER_TYPE_MASK;
					int dir_in = (ep->bEndpointAddress & LIBUSB_ENDPOINT_DIR_MASK) == LIBUSB_ENDPOINT_IN;
					if (type == LIBUSB_TRANSFER_TYPE_BULK && dir_in && !in) {
						in = ep->bEndpointAddress;
						mps = ep->wMaxPacketSize;
					} else if (type == LIBUSB_TRANSFER_TYPE_BULK && !dir_in && !out) out = ep->bEndpointAddress;
					else if (type == LIBUSB_TRANSFER_TYPE_INTERRUPT && dir_in && !irq) irq = ep->bEndpointAddress;
				}
				if (!in || !out) continue;
				GPPortSettingsUSB *s = &port->settings.usb;
				s->config = cfg->bConfigurationValue;
				s->interface = d->bInterfaceNumber;
				s->altsetting = d->bAlternateSetting;
				s->inep = in;
				s->outep = out;
				s->intep = irq;
				s->maxpacketsize = mps;
				return GP_OK;
			}
	return GP_ERROR_IO_USB_FIND;
}

static int
fail_open (GPPort *port, int code, const char *fmt, const char *detail)
{
	gp_port_set_error (port, fmt, detail);
	gp_log (GP_LOG_ERROR, PRISM_USBHOST_DOMAIN, fmt, detail);
	usbhost_close (port);
	return code;
}

static int
usbhost_open (GPPort *port)
{
	GPPortPrivateLibrary *pl = port->pl;
	if (!pl || !pl->t) {
		gp_port_set_error (port, "prism_usbhost: no USB Host transport in this process");
		return GP_ERROR_NOT_SUPPORTED;
	}
	if (pl->dh) return GP_OK;
	long id = device_of_path (port->settings.usb.port);
	if (id < 0) {
		gp_port_set_error (port, "prism_usbhost: %s is not a USB Host port", port->settings.usb.port);
		return GP_ERROR_BAD_PARAMETERS;
	}
	char why[256] = { 0 };
	int fd = pl->t->open ((uint32_t)id, port->timeout > 0 ? port->timeout : 60000, why, sizeof (why) - 1);
	if (fd < 0) {
		int code = fd == PRISM_USBHOST_ERR_DENIED ? GP_ERROR_IO_LOCK
		         : fd == PRISM_USBHOST_ERR_OPEN ? GP_ERROR_IO_USB_CLAIM : GP_ERROR_IO_USB_FIND;
		gp_port_set_error (port, "prism_usbhost: %s", why[0] ? why : "the camera could not be opened");
		return code;
	}
	pl->device_id = (uint32_t)id;
	pl->is_open = 1;

	struct libusb_init_option opt = { .option = LIBUSB_OPTION_NO_DEVICE_DISCOVERY };
	int r = libusb_init_context (&pl->ctx, &opt, 1);
	if (r != LIBUSB_SUCCESS) {
		pl->ctx = NULL;
		return fail_open (port, GP_ERROR_IO, "prism_usbhost: libusb could not start (%s)", libusb_error_name (r));
	}
	r = libusb_wrap_sys_device (pl->ctx, (intptr_t)fd, &pl->dh);
	if (r != LIBUSB_SUCCESS || !pl->dh) {
		pl->dh = NULL;
		return fail_open (port, prism_usbhost_map_error (r, GP_ERROR_IO),
		                  "prism_usbhost: the USB connection is not a usable usbfs device (libusb_wrap_sys_device: %s)",
		                  libusb_error_name (r));
	}
	struct libusb_config_descriptor *cfg = NULL;
	r = libusb_get_active_config_descriptor (libusb_get_device (pl->dh), &cfg);
	if (r != LIBUSB_SUCCESS || !cfg)
		return fail_open (port, GP_ERROR_IO, "prism_usbhost: no USB configuration descriptor (%s)",
		                  libusb_error_name (r));
	r = choose_interface (port, cfg);
	libusb_free_config_descriptor (cfg);
	if (r != GP_OK)
		return fail_open (port, GP_ERROR_IO_USB_FIND, "prism_usbhost: %s",
		                  "the camera has no interface with bulk in and out endpoints");
	r = libusb_claim_interface (pl->dh, port->settings.usb.interface);
	if (r != LIBUSB_SUCCESS)
		return fail_open (port, GP_ERROR_IO_USB_CLAIM, "prism_usbhost: could not claim the camera interface (%s)",
		                  libusb_error_name (r));
	pl->claimed = 1;
	if (port->settings.usb.altsetting) libusb_set_interface_alt_setting (pl->dh, port->settings.usb.interface,
	                                                                     port->settings.usb.altsetting);
	/* gp_port_set_settings copia settings_pending: che porti gli stessi endpoint. */
	port->settings_pending.usb = port->settings.usb;
	gp_log (GP_LOG_DEBUG, PRISM_USBHOST_DOMAIN, "device %u open: interface %d, in 0x%02x, out 0x%02x, int 0x%02x, mps %d",
	        pl->device_id, port->settings.usb.interface, port->settings.usb.inep, port->settings.usb.outep,
	        port->settings.usb.intep, port->settings.usb.maxpacketsize);
	return GP_OK;
}

static int
usbhost_close (GPPort *port)
{
	GPPortPrivateLibrary *pl = port->pl;
	if (!pl) return GP_OK;
	if (pl->dh) {
		if (pl->claimed) libusb_release_interface (pl->dh, port->settings.usb.interface);
		libusb_close (pl->dh);   /* fd_keep: libusb non chiude un fd avvolto */
	}
	if (pl->ctx) libusb_exit (pl->ctx);
	if (pl->is_open && pl->t) pl->t->close (pl->device_id);
	pl->dh = NULL;
	pl->ctx = NULL;
	pl->claimed = 0;
	pl->is_open = 0;
	return GP_OK;
}

GPPortOperations *
gp_port_library_operations (void)
{
	GPPortOperations *ops = calloc (1, sizeof (GPPortOperations));
	if (!ops) return NULL;
	ops->init = usbhost_init;
	ops->exit = usbhost_exit;
	ops->open = usbhost_open;
	ops->close = usbhost_close;
	ops->find_device = usbhost_find_device;
	ops->find_device_by_class = usbhost_find_device_by_class;
	prism_usbhost_io_operations (ops);
	return ops;
}
