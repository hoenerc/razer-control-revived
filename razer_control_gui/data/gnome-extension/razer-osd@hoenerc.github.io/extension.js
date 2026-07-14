// Companion extension for razer-control-revived.
//
// WHY THIS EXISTS: GNOME Shell restricts org.gnome.Shell.ShowOSD to an
// allowlist of gnome-settings-daemon bus names (DBusSenderChecker answers
// ACCESS_DENIED "ShowOSD is not allowed" — measured on Fedora 44 / GNOME 50,
// 2026-07-11). The native OSD pill is the only feedback surface that renders
// above fullscreen games, so this extension exports a tiny D-Bus service
// INSIDE the shell and forwards to Main.osdWindowManager. The daemon's
// power-key feedback calls it as stage 0; without the extension the cascade
// degrades to a transient notification.

import Gio from 'gi://Gio';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'io.github.hoenerc.RazerOSD';
const OBJECT_PATH = '/io/github/hoenerc/RazerOSD';

const IFACE_XML = `
<node>
  <interface name="io.github.hoenerc.RazerOSD">
    <method name="Show">
      <arg type="s" direction="in" name="icon"/>
      <arg type="s" direction="in" name="label"/>
    </method>
  </interface>
</node>`;

export default class RazerOsdExtension extends Extension {
    enable() {
        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._dbus.export(Gio.DBus.session, OBJECT_PATH);
        this._nameId = Gio.bus_own_name(
            Gio.BusType.SESSION, BUS_NAME, Gio.BusNameOwnerFlags.NONE,
            null, null, null);
    }

    disable() {
        if (this._nameId) {
            Gio.bus_unown_name(this._nameId);
            this._nameId = 0;
        }
        this._dbus?.unexport();
        this._dbus = null;
    }

    Show(icon, label) {
        const gicon = Gio.Icon.new_for_string(icon);
        const mgr = Main.osdWindowManager;
        if (mgr.showAll)
            mgr.showAll(gicon, label);   // GNOME 49+: all monitors
        else
            mgr.show(-1, gicon, label);  // GNOME 45-48: -1 = all monitors
    }
}
