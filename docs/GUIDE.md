# Using matter-peek

The web UI is served by `matter-peek serve`, and is also shown by the app. It
has one page for each task: **Devices**, **Commission**, **Updates**,
**Thread** and **Assistant**. The CLI and the MCP tools offer the same
functions. `matter-peek --help` lists the CLI commands, and [AI.md](AI.md)
lists the MCP tools.

## Adding devices

New devices can be commissioned over IP, or over Bluetooth LE. When a device is
commissioned over Bluetooth LE, matter-peek also gives it the Wi-Fi or Thread
network it should join.

- In the web UI, use the **Commission** page.
- In the CLI, use `matter-peek commission <code> --ip <addr>`, or
  `matter-peek commission <code> --ble`. With `--ble`, the network to join is
  given with `--wifi <ssid> --wifi-password <pw>` or with
  `--thread-dataset <hex>`.

The code can be the passcode, the manual pairing code or the `MT:` payload of
the QR code. The web UI can also scan the QR code with a camera; this works
over https or on localhost. In the CLI, `matter-peek qr <picture>` reads the QR
code from an image, and `matter-peek discover` lists the commissionable
devices nearby.

A device that is already used with another controller can be added through
multi-admin. First, have that controller open a pairing window for the device
(Google Home: *Linked Matter apps & services*; Apple Home: *Turn On Pairing
Mode*; Home Assistant: *Share device*). Then commission the device with the
code that the controller shows. `matter-peek advertised` lists the
commissioned devices that are advertised on the network, both ours and those of
other controllers.

Bluetooth commissioning needs BlueZ. It works on Linux, but not yet in the
Android app.

## Health, logs and updates

On the **Devices** page, each device has a card that is updated live. It shows
the uptime, the reboots and the operating hours. From the card you can also
open the device's networks, its memory (the heap, and how much stack each task
has left) and its logs.

In the CLI:

- `matter-peek peek <node>` reads the health of a device, and
  `matter-peek watch <node>` follows it live.
- `matter-peek logs <node> [crash|support|network]` retrieves a diagnostic log.
- `matter-peek network <node>` shows the networks a device knows, and can
  change them.

matter-peek acts as the OTA Provider for its devices. To update a device, add
an image on the **Updates** page, or with `matter-peek ota upload <file>`. The
cards of the devices that the image is meant for then offer the update, and
show its progress. In the CLI, `matter-peek ota update <node> <image>` starts
the update.

## Sleepy devices

Some battery devices (ICDs) sleep most of the time, and answer only while they
are awake. A device that can sleep long does so only while a controller is
registered for its Check-Ins. Otherwise it wakes up often, and answers within
seconds. When matter-peek commissions such a device, it registers for its
Check-Ins. The device then checks in with matter-peek each time it wakes up,
and matter-peek uses that time to talk to it.

While such a device sleeps, its card says *Asleep*, how often the device wakes
up, and how to wake it now, if the device tells (for example, by pressing a
button). Anything that needs the device waits until it wakes up. The sleep pill
in the card's header is also a switch: it keeps the device awake once it wakes
up, for 10 minutes at most. The **Check-Ins** switch on the card registers or
unregisters matter-peek.

In the CLI:

- `matter-peek icd register <node>` and `matter-peek icd unregister <node>`
  register and unregister for the device's Check-Ins.
- `matter-peek icd wake <node>` waits for the device to wake up, and keeps it
  awake for a while.
- `matter-peek icd stay-awake <node>` keeps it awake until it is interrupted,
  for 10 minutes at most.

Over MCP, use `register_check_ins`, `unregister_check_ins`, `wake_device` and
`stay_awake`.

## Filtering devices

The filter box of the web UI, `matter-peek list <filter>` and the `filter`
argument of the MCP tool `list_devices` all use the same filter language:

- A plain word matches a device if anything known about it has a word that
  starts with it: its tags, its names, or the values of its facets.
- A `key:value` term matches one facet, for example `net:thread is:offline`,
  `-eco:google cluster:on-off`, `type:light spec:>=1.4`, `uptime:<1h` or
  `tag:"ground floor"`.
- `key:a,b` matches either value, and `-term` negates a term.

`matter-peek filters` lists the facets and the `is:` states. The facets include
the network, the SSID and Thread network, the device type, vendor, product,
clusters, power, ICD, versions, the other controllers the device is on,
uptime, heap and update. The states include online, shared, fabrics-full,
updatable and sleepy.

The filter box in the web UI completes the last term, and shows how many
devices have each value. A card's network icon, device type, vendor and tags
add their term to the filter.

## Fabrics: several homes

matter-peek keeps its devices on fabrics of its own. Each fabric has its own
root CA and a name. Fabrics can be used to keep homes, customers, or test and
production devices apart. One fabric is the default one, and it cannot be
deleted.

The web UI shows one fabric at a time. You pick it at the top of the page, and
the browser remembers the choice. The selected fabric decides which devices and
which Thread mesh are shown, and which fabric a newly commissioned device is
added to.

In the CLI, commands work on the fabric given with `--fabric <name|#index|all>`
or with the `MATTER_PEEK_FABRIC` environment variable, and otherwise on the
default fabric. Fabrics are managed with
`matter-peek fabric list|create|rename|default|delete`. To delete a fabric that
still has devices, add `--decommission` or `--forget`.

Over MCP, there are the tools `list_fabrics`, `create_fabric`, `rename_fabric`,
`set_default_fabric` and `delete_fabric`. The tools that list or commission
devices take a `fabric` argument, and `matter-peek mcp --fabric` sets its
default.

Node IDs are unique across all fabrics. Every command or tool that works on a
single device therefore identifies it by its node ID alone.

## Sharing a device with another ecosystem

A device on our fabric can also be added to another controller, such as Google
Home, Home Assistant or Apple Home. For this, matter-peek opens an enhanced
commissioning window on the device, with a passcode created for that window
only. It then shows the code to commission with: the 11-digit manual pairing
code and the QR code. The QR code is drawn both in the web UI and in the
terminal.

- In the web UI, use the **Share** button on the device card.
- In the CLI, use `matter-peek share <node> [--minutes 3..15]`. `--status`
  shows the state of the window, and `--close` closes it.
- Over MCP, use `open_commissioning_window`, `close_commissioning_window` and
  `commissioning_window`.

The window closes after 15 minutes (any time from 3 to 15 minutes can be
chosen), or when it is closed. The code only works while the window is open. A
device that has no free fabric is refused.

The fabrics a device is on are shown together with its health. These are the
controllers the device belongs to: ours, Google, Apple, Home Assistant and
others, named by their vendor or by the label their controller set. They are
shown live on the device card, where a click shows each fabric's vendor, its
fabric ID and the device's node ID on it. They are also shown by
`matter-peek peek <node>` and by the MCP tool `peek_device`.

Another controller's fabric can be removed from a device: in the pop-up, with
`matter-peek remove-fabric <node> <index>`, or with the MCP tool
`remove_device_fabric`. Our own fabric is removed by decommissioning the
device.

## Time: a time source for our devices

matter-peek serves this host's clock to the devices on its fabrics, through the
Time Synchronization cluster on its own node. It also sets the clocks of the
devices:

- their UTC time;
- their time zone and daylight saving times, taken from this host's time zone,
  if the device keeps them;
- matter-peek itself as their trusted time source, if the device takes its
  time from a Matter node. The device then reads the time from matter-peek.

matter-peek does this once a device is commissioned, but it leaves another
controller's time source in place. It can also be done on demand: with the
**Clock** button on the device card, with `matter-peek time <node> [--set]`,
or with the MCP tools `device_time` and `set_device_time`.

Reading a device's clock shows its time and how far it is from ours, its
accuracy and its time source, its time zone, its DST offset and its local
time.

## Thread mesh

The **Thread** page of the web UI, `matter-peek thread` and the MCP tool
`thread_mesh` show the Thread mesh as our Thread devices see it.
`matter-peek thread findings` and the MCP tool `thread_findings` show only the
findings.

The mesh is built from Matter data only, so no Thread credentials are needed.
matter-peek reads the Thread Network Diagnostics of each device (its neighbour
and route tables), merges them into one graph, and labels it with the border
routers' MeshCoP adverts (`_meshcop._udp`). The nodes are counted by role. For
each link, the link quality (0–3) and the signal are shown in both directions.

How a node sleeps is taken from its role, from the ICD Management cluster of
our devices, and from the `_matter._tcp` adverts (`SII`, `SAI` and `ICD`).
Whether a sleepy node uses CSL cannot be seen.

Matter nodes of other fabrics whose address is in a border router's prefix are
listed as seen on the LAN, together with their fabrics and how they sleep. They
are not placed in the mesh, because none of the tables that matter-peek reads
places them there.

The findings are described in plain words. matter-peek looks for:

- several Thread networks on the LAN;
- a network that is split;
- weak or one-way links;
- routers that some of our devices depend on;
- a single border router;
- devices that keep changing their parent, or keep losing the network;
- a Thread channel within a Wi-Fi channel that our Wi-Fi devices use;
- OpenThread's default credentials;
- no route from this machine to a network's prefix (on Linux).

matter-peek reads the devices one after another, so this takes a few seconds.
Only the links that our devices report are known.

## The data model

matter-peek shows every endpoint, cluster, attribute and command of a device
by name. Values are shown as JSON: enums by name, bitmaps as the names of their
bits, and structs by field name. Any cluster in rs-matter's IDL works, without
code for each cluster.

- In the web UI, the **Model** button on the device card opens a tree of
  endpoints and clusters. Writable attributes can be edited in place, and
  commands are invoked with a form for their fields.
- In the CLI, use
  `matter-peek model <node> [--endpoint N] [--cluster C] [--live]`,
  `matter-peek write` and `matter-peek invoke`.
- Over MCP, use `read_model`, `write_attribute` and `invoke_command`.

Events are shown too, such as switch presses, alarms, faults, boots and state
changes, with their fields by name. In the web UI they appear in the pop-up's
**Events** timeline and with each cluster. In the CLI, use
`matter-peek events <node> [--endpoint N] [--cluster C] [--live]`, and over
MCP, `read_events`.

A device card can also be turned over to show the controls of its application
endpoints, together with their latest event. The controls are switches,
sliders, choices, buttons and readings, with colour pickers for lights, lock
and charger switches, and remotes for TVs. The **Health | Controls** switch on
the Devices page turns all the shown cards at once. The controls, the events
and the Model pop-up follow the device live, through the same subscription as
its health.
