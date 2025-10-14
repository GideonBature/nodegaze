"use client";

import React, { useState, useEffect } from "react";
import { usePathname } from "next/navigation";

import {
  Sidebar,
  SidebarContent,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarRail,
} from "@/components/ui/sidebar";
import { Eye } from "@/public/assets/icons/eye";
import { Logo } from "@/public/assets/icons/logo";
import { Network } from "@/public/assets/icons/network";
import { Note } from "@/public/assets/icons/note";
import { Socket } from "@/public/assets/icons/socket";

interface NavigationItem {
  title: string;
  url: string;
  icon: React.ComponentType<{ className?: string }>;
  count?: number;
}

interface NavigationSection {
  title?: string;
  items: NavigationItem[];
}

export function AppSidebar({ ...props }: React.ComponentProps<typeof Sidebar>) {
  const pathname = usePathname();
  const [channelCount, setChannelCount] = useState(0);
  const [eventCount, setEventCount] = useState(0);

  useEffect(() => {
    const fetchChannelCount = async () => {
      try {
        const res = await fetch("/api/channels?page=1&per_page=10");
        const result = await res.json();
        setChannelCount(result?.data?.items?.length || 0);
      } catch {
        setChannelCount(0);
      }
    };
    fetchChannelCount();
  }, []);

  useEffect(() => {
    const fetchEventCount = async () => {
      try {
        const res = await fetch("/api/events?page=1&per_page=10");
        const result = await res.json();
        setEventCount(result?.data?.items?.length || 0);
      } catch {
        setEventCount(0);
      }
    };
    fetchEventCount();
  }, []);



  const navigationItems: NavigationSection[] = [
    {
      title: "Dashboard",
      items: [
        { title: "Overview", 
          url: "/overview", 
          icon: Eye, 
        },
        {
          title: "Channels",
          url: "/channels",
          icon: Socket,
          count: channelCount,
        },
        { title: "Events", 
          url: "/events", 
          icon: Network, 
          count: eventCount,
        },
      ],
    },
    {
      title: "Transactions",
      items: [
        { title: "Payment", 
          url: "/payments", 
          icon: Note, 
        },
      ],
    },
  ];

  return (
    <Sidebar {...props}>
      <SidebarContent className="px-4">
        <section className="flex gap-5 my-7 items-center justify-left">
          <Logo className="w-10 h-10" />
          <span className="text-[25.83px] font-semibold text-blue-primary font-lato">
            Nodegaze
          </span>
        </section>
        {navigationItems.map((section, index) => (
          <SidebarGroup key={index}>
            {section.title && (
              <SidebarGroupLabel className="text-sm font-medium text-grey-accent font-clash-grotesk">
                {section.title}
              </SidebarGroupLabel>
            )}
            <SidebarGroupContent>
              <SidebarMenu>
                {section.items.map((item) => {
                  const isActive = pathname === item.url;
                  return (
                    <SidebarMenuItem key={item.title}>
                      <SidebarMenuButton
                        asChild
                        isActive={isActive}
                        className="text-sm h-11 font-medium font-clash-grotesk"
                        disabled={
                          item.url === "/payments" || item.url === "/events"
                        }
                      >
                        <a
                          href={item.url}
                          className={`flex items-center justify-between ${
                            ["/payments", "/events"].includes(
                              item.url
                            )
                              ? ""
                              : ""
                          }`}
                        >
                          <div className="flex items-center gap-2">
                            <item.icon className="h-4 w-4" />
                            <span>{item?.title}</span>
                          </div>
                          {item.count !== undefined && (
                            <span className="bg-grey-sub-background rounded-xl px-2 py-1 text-xs font-clash-grotesk font-medium text-grey-primary">
                              {item.count}
                            </span>
                          )}
                        </a>
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                  );
                })}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        ))}
      </SidebarContent>
      <SidebarRail />
    </Sidebar>
  );
}
