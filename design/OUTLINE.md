# Mechanic: a terminal interface

## General

Mechanic is a terminal interface and terminal emulator written in Rust.
Our goal is an application that will run on Mac, initially x86 Macs.

I would like this to replace programs like Mac's own Terminal or iTerm2 for my daily use.

## Design features

Mechanic should be able to operate as a terminal emulator and interact with Mac's UNIX / bash shell.

Mechanic should have multilanguage support, including Latin-1, and allow input in the following languages and character sets:

* English
* French
* Spanish
* Russian
* Arabic (including bidi support)

## Cosmetic and other features

Mechanic should be able to have floating subwindows, as many X-windows derived systems have.

Mechanic terminal windows should have a title bar that is 95% opaque, and a main window / typing area that begins at 80% opaque (semi-transparent) and moves to 95% opaque during a period of user interaction over 30 seconds. It then fades gradually, starting after 30 seconds of inactivity, and hitting 80% at 1 minute.

The initial color palette should be inspired by the Stark computer interfaces, so high-constrast cyan, blue, and electric white. We will want the color palette adjustable after v1.

Examples of colors: electric #52E8FF, celeste #ADFFFF, azure #007FFF, blue #0015FF

Highlight colors, or colors for displaying things like file folders, can be shades of orange and yellow.
An orange-red should be used for alerts.

The default font should be Berkeley Mono, local to my machine, but we don't wish to distribute this.
Font, like color, will need to be adjustable eventually.

The background of the window should be black, with a faint, animated gradient tinge at the lower right-hand corner.

## Dream features

When switching between terminal windows, the Mechanic windows should "tilt in," rotate, and have a slight perspective view, to appear as if they've rotated 45 degrees toward the center of the screen and shrunk slightly, allowing the user to cycle through those windows. A text overlay would then show the titles of each of those bars to assist in navigation.

When a new subwindow is requested, it should be generated through an animation that appears at the top of the screen, and moves the new window from 25% target size and 25% opacity to 100% size and the standard 85% opacity in 1000 ms, as it travels to its screen location.

## Possible technologies

The code will be in Rust.
We should evaluate alacritty, egui, bevy, egui_term, and others to reach our goals.
It is fine to fork a repo and move from there.
