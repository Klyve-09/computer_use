/* DISPOSABLE DESIGN PROBE. No buttons or keyboard events are sent.
 * With no args: inspect protocol availability. With x y width height:
 * send one absolute pointer move and report compositor roundtrip status.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wayland-client.h>
#include "virtual-pointer.h"
static struct zwlr_virtual_pointer_manager_v1 *manager;
static void global(void *data, struct wl_registry *registry, uint32_t name,
                   const char *interface, uint32_t version) {
    (void)data;
    if (!strcmp(interface, "zwlr_virtual_pointer_manager_v1")) {
        printf("virtual_pointer_version=%u\n", version);
        manager=wl_registry_bind(registry,name,&zwlr_virtual_pointer_manager_v1_interface,version>2?2:version);
    }
}
static void removed(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data; (void)registry; (void)name;
}
int main(int argc, char **argv) {
    struct wl_display *display=wl_display_connect(NULL);
    if (!display) return 2;
    struct wl_registry *registry=wl_display_get_registry(display);
    const struct wl_registry_listener listener={global,removed};
    wl_registry_add_listener(registry,&listener,NULL);
    if (wl_display_roundtrip(display)<0 || !manager) return 3;
    if (argc==5) {
        struct zwlr_virtual_pointer_v1 *pointer=zwlr_virtual_pointer_manager_v1_create_virtual_pointer(manager,NULL);
        zwlr_virtual_pointer_v1_motion_absolute(pointer,0,strtoul(argv[1],NULL,10),strtoul(argv[2],NULL,10),strtoul(argv[3],NULL,10),strtoul(argv[4],NULL,10));
        zwlr_virtual_pointer_v1_frame(pointer);
        printf("motion_roundtrip=%d\n",wl_display_roundtrip(display));
        zwlr_virtual_pointer_v1_destroy(pointer);
        wl_display_roundtrip(display);
    }
    zwlr_virtual_pointer_manager_v1_destroy(manager);
    wl_registry_destroy(registry);
    wl_display_disconnect(display);
    return 0;
}
