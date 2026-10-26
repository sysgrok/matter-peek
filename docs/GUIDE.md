# Using matter-peek

The web UI (`matter-peek serve`, or the app) has a page for each job:
**Devices**, **Commission**, **Updates**, **Thread** and the **Assistant**.
The CLI and the MCP tools do the same: `matter-peek --help` lists the
commands, and [AI.md](AI.md) the tools.

## Getting devices in

Commission a new device over IP, or over Bluetooth LE, handing it the Wi-Fi or
Thread network to join: from the Commission page, or `matter-peek commission
<code> --ip <addr>` / `--ble [--wifi <ssid> --wifi-password <pw> |
--thread-dataset <hex>]`. The code can be the passcode, the manual pairing code
or the QR code's `MT:` payload. The web UI can also scan the QR code with a
camera (over https or on localhost), and `matter-peek qr <picture>` reads it
from an image. `matter-peek discover` lists the commissionable devices around.

A device already on another controller joins ours through multi-admin: have
that controller open a pairing window (Google Home: *Linked Matter apps &
services*; Apple Home: *Turn On Pairing Mode*; Home Assistant: *Share device*)
and commission it with the code it shows. `matter-peek advertised` lists the
commissioned devices the network advertises, ours and others'.

Bluetooth commissioning needs BlueZ: it works on Linux, but not in the
Android app yet.

## Health, logs and updates

Each device card on the Devices page follows its device live: uptime, reboots
and operating hours, plus the networks, memory (heap and the tasks' stack
headroom) and logs one click away. In the terminal, use `matter-peek peek
<node>` and `matter-peek watch <node>`. `matter-peek logs <node>
[crash|support|network]` retrieves a diagnostic log, and `matter-peek network
<node>` shows the networks a device knows and changes them.

matter-peek is its devices' OTA Provider. Drop an image on the Updates page
(or `matter-peek ota upload <file>`), and the cards of the devices it fits
offer the update and follow its progress (`matter-peek ota update <node>
<image>`).

## Filtering devices

One filter language for the web UI's filter box, `matter-peek list <filter>`
and MCP's `list_devices` `filter`: words match the start of a word of
anything known of a device (tags, names, the facets' values), and
`key:value` terms match one facet: `net:thread is:offline`,
`-eco:google cluster:on-off`, `type:light spec:>=1.4`, `uptime:<1h`,
`tag:"ground floor"`. `key:a,b` takes either value and `-term` negates.
`matter-peek filters` lists the facets (network, SSID and Thread network,
device type, vendor, product, clusters, power, ICD, versions, the other
controllers it is on, uptime, heap, update, ...) and the `is:` states
(online, shared, fabrics-full, updatable, sleepy, ...). The web box completes
the last term, with how many devices have each value; a card's network icon,
type, vendor and tags add their term.

## Fabrics: several homes

matter-peek keeps devices on fabrics of its own, each with a root CA of its
own and a name: homes, customers, or test and production apart. One is the
default, and can't be deleted. The web UI shows one fabric at a time (picked
at the top, remembered by the browser): its devices, the Thread mesh of its
devices, and the fabric a commission puts a device into. The CLI works in
`--fabric <name|#index|all>` (or `MATTER_PEEK_FABRIC`), else the default, and
manages them with `matter-peek fabric list|create|rename|default|delete`
(`delete --decommission` or `--forget` for a fabric with devices); MCP has
`list_fabrics` and `create/rename/set_default/delete_fabric`, and a `fabric`
argument on the tools that list or commission (`mcp --fabric` sets the
default). Devices keep their node IDs unique across fabrics, so every tool
about one device names it by node ID alone.

## Sharing a device with another ecosystem

A device on our fabric can join another controller's too (Google Home, Home
Assistant, Apple Home, ...): the card's Share button, `matter-peek share
<node> [--minutes 3..15]` (`--status`, `--close`) and the MCP tools
`open_commissioning_window` / `close_commissioning_window` /
`commissioning_window` open an enhanced commissioning window on it, with a
passcode made for that window, and show the code to commission it with: the
11-digit manual pairing code and the QR code (drawn in the web UI and the
terminal). The window closes after 15 minutes (3 to 15), or when closed; the
code works only while it is open. A device with no free fabric is refused.

The fabrics a device is on (the controllers it belongs to: ours, Google,
Apple, Home Assistant, ... by their vendor, or the label their controller set)
come with its health: live on the device card (a click shows each one's
vendor, fabric ID and the device's node ID there), in `matter-peek peek
<node>` and in the MCP tool `peek_device`. Another controller's fabric can be
removed from a device (the popup, `matter-peek remove-fabric <node> <index>`,
MCP `remove_device_fabric`); ours is removed by decommissioning.

## Time: a time source for our devices

matter-peek serves this host's clock to the devices on its fabrics (the Time
Synchronization cluster on its node), and sets their clocks from it: their UTC
time; their time zone and daylight saving times, from this host's zone, if
they keep them; and itself as their trusted time source, if they take their
time from a Matter node, which they then read it from. It does so once a
device is commissioned (leaving another controller's time source in place),
and on demand: the card's Clock button, `matter-peek time <node> [--set]`, and
the MCP tools `device_time` / `set_device_time`. Reading a device's clock
shows its time and how far it is from ours, its accuracy and source, its zone,
DST and local time.

## Thread mesh

The Thread page of the web UI, `matter-peek thread` (`thread findings` for the
findings alone) and the MCP tools `thread_mesh` / `thread_findings` show the
Thread mesh as our Thread devices see it, from Matter data only (no Thread
credentials): each device's Thread Network Diagnostics (its neighbour and
route tables), merged into one graph, labelled with the border routers' MeshCoP
adverts (`_meshcop._udp`). The nodes are counted by role, and the links show
the link quality (0–3) and signal each way. How a node sleeps comes from its
role, our devices' ICD Management cluster and the `_matter._tcp` adverts
(`SII`/`SAI`/`ICD`); whether a sleepy node uses CSL is not visible. Other
fabrics' Matter nodes whose address is in a border router's prefix are listed
as seen on the LAN, with their fabrics and how they sleep: no table we read
places them in the mesh. The findings, in plain words: several
networks on the LAN, a split network, weak or one-way links, routers some of
our devices depend on, a single border router, devices that keep changing
their parent or losing the network, a Thread channel within a Wi-Fi channel
our Wi-Fi devices use, OpenThread's default credentials, and no route from
this machine to a network's prefix (Linux). It reads the devices one after
another, so it takes a few seconds; only the links our devices report are
known.

## The data model

Every endpoint, cluster, attribute and command of a device, by name, with
values as JSON (enums by name, bitmaps as bit names, structs by field name):
the card's Model button (a tree of endpoints and clusters; writable attributes
edited in place, commands invoked with a form of their fields), `matter-peek
model <node> [--endpoint N] [--cluster C] [--live]`, `matter-peek write` /
`invoke`, and the MCP tools `read_model` / `write_attribute` /
`invoke_command`. Any cluster in rs-matter's IDL works, with no code per
cluster.

Its events too (switch presses, alarms, faults, boots, state changes), with
their fields by name: the popup's Events timeline and each cluster's events,
`matter-peek events <node> [--endpoint N] [--cluster C] [--live]`, and the
MCP tool `read_events`.

A device card turns over to the controls of its application endpoints
(switches, sliders, choices, buttons and readings, with colour pickers for
lights, lock and charger switches, remotes for TVs) and their latest event;
the Health | Controls switch on the Devices page turns all the cards shown.
The controls, the events and the Model popup follow the device live, through
the same subscription as its health.
