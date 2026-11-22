/*
 * prism_usbhost_transport.h — il contratto C fra l'iolib prism_usbhost (libgphoto2_iolib_prism_usbhost.so) e l'app
 * (M8.1, docs/ANDROID-GPHOTO2.md). Sul modello di prism_wpd_transport.h (Windows) e prism_icc_transport.h (iPad).
 *
 * Su Android un'app non apre /dev/bus/usb: UsbManager (Kotlin) chiede il permesso all'utente, apre il device e da'
 * un file descriptor di usbfs (UsbDeviceConnection.getFileDescriptor()). L'app registra questa tabella nell'iolib e
 * l'iolib ne fa una porta USB VERA: libusb_wrap_sys_device(fd) e trasferimenti bulk / interrupt / control sul filo;
 * il camlib ptp2 (camlibs/ptp2/usb.c) parla PTP com'e', OpenSession compresa.
 *
 * La tabella NON si cerca con dlsym(RTLD_DEFAULT, …) come fanno prism_wpd / prism_icc: su Android libprism.so e'
 * caricata da System.loadLibrary senza RTLD_GLOBAL e RTLD_DEFAULT non la vede. L'app fa dlopen dell'iolib per nome,
 * TIENE l'handle aperto (libltdl chiude l'iolib dopo l'elenco delle porte e bionic la scaricherebbe, perdendo la
 * registrazione) e chiama prism_usbhost_register_transport. Quando libltdl riapre l'iolib dalla cartella dei
 * collegamenti, il linker riconosce lo stesso file (stesso inode) e da' lo stesso modulo: la tabella c'e'.
 *
 * Thread: list e open dal thread di libgphoto2 (il worker della sessione); open PUO' bloccare (permesso dell'utente).
 * Licenza: codice di Prism; l'iolib si linka DINAMICAMENTE a libgphoto2_port e a libusb (LGPL 2.1+).
 */
#ifndef PRISM_USBHOST_TRANSPORT_H
#define PRISM_USBHOST_TRANSPORT_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define PRISM_USBHOST_TRANSPORT_VERSION 1u
/* «usb:USBHOST,<deviceId>»: deviceId = UsbDevice.getDeviceId(). */
#define PRISM_USBHOST_PREFIX "usb:USBHOST,"
#define PRISM_USBHOST_DOMAIN "prism_usbhost"
#define PRISM_USBHOST_REGISTER_SYMBOL "prism_usbhost_register_transport"
#define PRISM_USBHOST_MAX_DEVICES 16

typedef struct prism_usbhost_device_info {
	uint32_t device_id;          /* UsbDevice.getDeviceId() */
	uint16_t vendor_id, product_id;
	/* La classe dell'interfaccia di cattura (UsbInterface): 6/1/1 per PTP Still Image. 0/0/0 se non nota: allora
	 * gp_abilities_list_detect abbina solo per vendor / product. */
	uint8_t iface_class, iface_subclass, iface_protocol;
	uint8_t permitted;           /* UsbManager.hasPermission adesso (solo informativo: open chiede se serve) */
	char name[128];              /* «ILCE-1», «X-Pro2» (getProductName) */
} prism_usbhost_device_info;

/* Esiti negativi di open (il resto: un fd >= 0). */
#define PRISM_USBHOST_ERR_NO_DEVICE   (-1)   /* staccata, o il deviceId non c'e' piu' */
#define PRISM_USBHOST_ERR_DENIED      (-2)   /* l'utente ha detto no al permesso USB (o non ha risposto in tempo) */
#define PRISM_USBHOST_ERR_OPEN        (-3)   /* openDevice / claimInterface rifiutati */

typedef struct prism_usbhost_transport {
	uint32_t version;   /* PRISM_USBHOST_TRANSPORT_VERSION */
	/* Le camere collegate adesso (al piu' capacity). Non chiede permessi, non apre niente. */
	int (*list) (prism_usbhost_device_info *out, int capacity);
	/* Il permesso (se manca, lo chiede e aspetta al piu' timeout_ms), poi l'fd di usbfs con l'interfaccia PTP gia'
	 * reclamata. L'fd resta del guscio: l'iolib non lo chiude MAI. Negativo (PRISM_USBHOST_ERR_*) + `why` (una frase
	 * inglese, troncata a why_size) se no. */
	int (*open) (uint32_t device_id, int timeout_ms, char *why, size_t why_size);
	/* UsbDeviceConnection.releaseInterface + close (dopo che l'iolib ha rilasciato l'handle libusb). */
	void (*close) (uint32_t device_id);
} prism_usbhost_transport;

/* Esportata dall'iolib. NULL toglie la tabella. Una tabella di un'altra versione e' rifiutata (log). */
void prism_usbhost_register_transport (const prism_usbhost_transport *transport);
typedef void (*prism_usbhost_register_fn) (const prism_usbhost_transport *transport);

#ifdef __cplusplus
}
#endif

#endif /* PRISM_USBHOST_TRANSPORT_H */
