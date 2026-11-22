/*
 * prism_usbhost_private.h — lo stato di una porta dell'iolib prism_usbhost, condiviso fra prism_usbhost.c (elenco,
 * apertura, descrittori) e prism_usbhost_io.c (i trasferimenti). Non e' un'API: solo per i due file dell'iolib.
 */
#ifndef PRISM_USBHOST_PRIVATE_H
#define PRISM_USBHOST_PRIVATE_H

#include <gphoto2/gphoto2-port-library.h>
#include <gphoto2/gphoto2-port.h>
#include <gphoto2/gphoto2-port-result.h>
#include <gphoto2/gphoto2-port-log.h>
#include <libusb.h>

#include "prism_usbhost_transport.h"

struct _GPPortPrivateLibrary {
	const prism_usbhost_transport *t;
	libusb_context *ctx;
	libusb_device_handle *dh;
	uint32_t device_id;
	int is_open;            /* t->open ha dato l'fd (da chiudere con t->close) */
	int claimed;            /* l'interfaccia reclamata da libusb_claim_interface */
	/* Che cosa ha chiesto find_device / find_device_by_class (gp_camera_init lo fa PRIMA di open). */
	int want_class, want_subclass, want_protocol;
};

/* L'esito di libusb come codice di libgphoto2 (come libusb1.c: translate_libusb_error). */
int prism_usbhost_map_error (int libusb_result, int fallback);

/* La tabella delle operazioni: prism_usbhost.c mette init / exit / open / close / find_*, prism_usbhost_io.c il resto. */
void prism_usbhost_io_operations (GPPortOperations *ops);

#endif /* PRISM_USBHOST_PRIVATE_H */
